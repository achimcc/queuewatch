//! bindery, the Readarr successor: books and audiobooks in the queue, and
//! recent imports.
//!
//! Field names are from the bindery source at tag v1.42.1:
//!
//! - `GET /api/v1/queue` (`QueueHandler.List`, `internal/api/queue.go`)
//!   answers `{"items": [...]}` — every active download, unpaged. An item is
//!   `models.Download` (`internal/models/download.go`) plus the live
//!   `percentage` and `timeLeft` strings of its client and a `book`
//!   projection (`internal/api/bookref.go`: `title`, `authorName`).
//! - `GET /api/v1/history` (`HistoryHandler.List`, `internal/api/history.go`)
//!   answers `{"items", "total", "limit", "offset"}`, newest first, paged
//!   with `limit` (at most 500) and `offset`. An import is the event type
//!   `bookImported`.
//! - The key goes in `X-Api-Key` (`requestAPIKey`,
//!   `internal/auth/middleware.go`); bindery would also take `?apikey=` on a
//!   GET, which puts the key into every access log on the way.
//!
//! What is NOT read here, deliberately: `errorMessage`, `nzbUrl`, `guid`
//! (a torznab GUID is the download URL, key and passkey included), the
//! history event's `data` (it carries the library path), `staleClients`
//! (client names and their error messages) and `quality`.
//!
//! bindery keeps no media type on a download — `grabMediaType` only picks the
//! client category from it — so whether a line is a book or an audiobook
//! comes from the format tokens in the release name, as bindery's own
//! `indexer.MediaTypeForFormat` does it.

use reqwest::Client;
use serde::Deserialize;
use url::Url;

use super::http::{self, FetchError};
use super::merge::{Contribution, Tracked};
use super::title::client_title;
use super::{Round, bytes, eta_in, parse_time};
use crate::snapshot::{ItemState, MediaIds, MediaKind, Origin, QueueItem, RecentItem, Transport};

pub struct Bindery {
    pub label: String,
    base: Url,
    key: String,
    http: Client,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct QueueList {
    items: Vec<Download>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Download {
    /// The release name: the fallback title for a download without a book,
    /// and where the format token is read from.
    title: Option<String>,
    size: Option<f64>,
    sabnzbd_nzo_id: Option<String>,
    torrent_id: Option<String>,
    status: Option<String>,
    protocol: Option<String>,
    /// The client's own number, `"42.0"` (torrent clients) or `"42"` (SABnzbd).
    percentage: Option<String>,
    /// `"1h 05m"`, `"3m 07s"`, `"45s"` (torrent clients), `"0:12:34"` (SABnzbd).
    time_left: Option<String>,
    book: Option<BookRef>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BookRef {
    title: Option<String>,
    author_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HistoryPage {
    items: Vec<HistoryEvent>,
    total: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct HistoryEvent {
    event_type: Option<String>,
    source_title: Option<String>,
    created_at: Option<String>,
    book: Option<BookRef>,
}

/// bindery's own maximum for `limit` (`historyListMaxLimit`).
const HISTORY_PAGE_SIZE: usize = 500;

/// Pages followed at most — the same bound as the managers' queue. Five
/// thousand imports inside `recent_hours` is a bulk import, not something to
/// list on a status page.
const HISTORY_MAX_PAGES: usize = 10;

/// The event bindery records once per imported download
/// (`models.HistoryEventBookImported`).
const IMPORT_EVENT: &str = "bookImported";

/// bindery's format tokens that mean an audiobook (`formatTokens` and
/// `IsAudiobookFormat`, `internal/indexer/release.go`). Every other token —
/// and no token at all — is a book.
const AUDIO_TOKENS: &[&str] = &["m4b", "m4a", "flac", "mp3", "ogg"];

fn text(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// "Andy Weir – Project Hail Mary"; the title alone without an author.
fn book_title(book: Option<&BookRef>) -> Option<String> {
    let book = book?;
    let title = text(&book.title)?;
    Some(match text(&book.author_name) {
        Some(author) => format!("{author} – {title}"),
        None => title.to_string(),
    })
}

/// Audiobook when an audio format token stands in the release name as a word
/// of its own. Unlike bindery's `ParseRelease`, which keeps only the first
/// token in list order, ANY audio token counts: "… M4B + PDF booklet" is an
/// audiobook (bindery's `ReleaseFormats` says the same).
fn media_kind(release: Option<&str>) -> MediaKind {
    let lower = release.unwrap_or("").to_lowercase();
    let audio = lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| AUDIO_TOKENS.contains(&word));
    if audio {
        MediaKind::Audiobook
    } else {
        MediaKind::Book
    }
}

/// `models.DownloadState` (`internal/models/download_state.go`) to a class.
/// `importFailed` is still being retried by bindery's scanner, but it is
/// the manager saying the import did not work: a warning, as with the *arr
/// apps. `importExternal` and `importHeld` hand the files to another tool or
/// wait for the other format — on their way into the library.
fn queue_state(status: Option<&str>) -> ItemState {
    match status.unwrap_or("") {
        "downloading" => ItemState::Downloading,
        "completed" | "importPending" | "importing" | "importExternal" | "importHeld" => {
            ItemState::Importing
        }
        "importFailed" | "importBlocked" => ItemState::Warning,
        "failed" => ItemState::Failed,
        "imported" => ItemState::Completed,
        // "grabbed": sent to the client, which has not reported it yet.
        _ => ItemState::Queued,
    }
}

fn transport(protocol: Option<&str>) -> Transport {
    match protocol {
        Some("usenet") => Transport::Usenet,
        Some("torrent") => Transport::Torrent,
        _ => Transport::Unknown,
    }
}

/// `"42.0"`, `"42"` or `"42%"` as 0.42; `None` for anything else.
fn percentage(raw: Option<&str>) -> Option<f32> {
    let raw = raw?.trim().trim_end_matches('%').trim();
    let p: f64 = raw.parse().ok()?;
    p.is_finite().then(|| (p / 100.0).clamp(0.0, 1.0) as f32)
}

/// Seconds from bindery's `timeLeft`: `"1h 05m"`, `"3m 07s"`, `"45s"`
/// (`etaToTimeLeft`, `internal/downloader/adapter.go`) or SABnzbd's own
/// `"0:12:34"`, which bindery passes on unchanged. Zero and garbage are no
/// time.
fn time_left(raw: Option<&str>) -> Option<f64> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let seconds = if raw.contains(':') {
        // `[d:]h:mm:ss`, read from the right.
        let parts: Vec<&str> = raw.split(':').collect();
        if parts.len() > 4 {
            return None;
        }
        let mut total = 0u64;
        for (part, factor) in parts.iter().rev().zip([1u64, 60, 3_600, 86_400]) {
            let n: u64 = part.parse().ok()?;
            total = total.checked_add(n.checked_mul(factor)?)?;
        }
        total
    } else {
        let mut total = 0u64;
        for word in raw.split_whitespace() {
            let unit = word.chars().last()?;
            let n: u64 = word[..word.len() - unit.len_utf8()].parse().ok()?;
            let factor = match unit {
                'd' => 86_400,
                'h' => 3_600,
                'm' => 60,
                's' => 1,
                _ => return None,
            };
            total = total.checked_add(n.checked_mul(factor)?)?;
        }
        total
    };
    (seconds > 0).then_some(seconds as f64)
}

impl Bindery {
    pub fn new(label: &str, url: &str, key: String, http: Client) -> anyhow::Result<Self> {
        Ok(Bindery {
            label: label.to_string(),
            base: http::base_url(url)?,
            key,
            http,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, FetchError> {
        let url = http::endpoint(&self.base, &format!("/api/v1{path}"), query);
        http::get_json(self.http.get(url).header("X-Api-Key", &self.key)).await
    }

    /// Imports since `round.recent_since`, page by page. The history is
    /// newest first and has no date filter, so a page whose oldest event is
    /// before the window is the last one needed; `total`, an empty page or
    /// [`HISTORY_MAX_PAGES`] stop it as well.
    async fn history(&self, round: &Round) -> Result<Vec<HistoryEvent>, FetchError> {
        let limit = HISTORY_PAGE_SIZE.to_string();
        let mut events = Vec::new();
        for page in 0..HISTORY_MAX_PAGES {
            let offset = (page * HISTORY_PAGE_SIZE).to_string();
            let answer: HistoryPage = self
                .get(
                    "/history",
                    &[
                        ("eventType", IMPORT_EVENT),
                        ("limit", limit.as_str()),
                        ("offset", offset.as_str()),
                    ],
                )
                .await?;
            let got = answer.items.len();
            let past_window = answer
                .items
                .iter()
                .filter_map(|e| parse_time(e.created_at.as_deref()))
                .any(|t| t < round.recent_since);
            events.extend(answer.items);
            let total = answer.total.unwrap_or(0).max(0) as usize;
            if got == 0 || past_window || events.len() >= total {
                return Ok(events);
            }
        }
        eprintln!(
            "queuewatch: bindery {:?}: the history has more than {} pages of imports; the rest is left out",
            self.label, HISTORY_MAX_PAGES
        );
        Ok(events)
    }

    pub async fn fetch(&self, round: &Round) -> Result<Contribution, FetchError> {
        let (queue, history) =
            tokio::try_join!(self.get::<QueueList>("/queue", &[]), self.history(round),)?;
        let mut part = Contribution::default();
        for d in &queue.items {
            if let Some(item) = queue_item(d, round) {
                part.managed.push(item);
            }
        }
        for e in &history {
            if let Some(item) = recent_item(e, round) {
                part.recent.push(item);
            }
        }
        Ok(part)
    }
}

fn queue_item(d: &Download, round: &Round) -> Option<Tracked> {
    let state = queue_state(d.status.as_deref());
    // bindery lists an import for a week after it happened; the page shows
    // it under "recently added" instead, like bindery's own *arr-style queue
    // (`includeArrQueueItem`).
    if state == ItemState::Completed {
        return None;
    }
    // A free-text grab has no book until the importer matches it, and its
    // release name may be the grab URL: through the filter.
    let title = book_title(d.book.as_ref()).or_else(|| client_title(d.title.as_deref()))?;
    let size = bytes(d.size).filter(|b| *b > 0);
    // Past the download bindery counts nothing as left (`queueItemSizeLeft`).
    let progress = if state == ItemState::Importing || state == ItemState::Warning {
        Some(1.0)
    } else {
        percentage(d.percentage.as_deref())
    };
    let left = match (size, progress) {
        (Some(size), Some(p)) => Some((size as f64 * (1.0 - f64::from(p))).round() as u64),
        _ => None,
    };
    // The id the download client knows it by: the torrent hash, else the
    // nzo id (`storedDownloadID`).
    let download_id = text(&d.torrent_id)
        .map(str::to_lowercase)
        .or_else(|| text(&d.sabnzbd_nzo_id).map(String::from));
    Some(Tracked {
        item: QueueItem {
            kind: media_kind(d.title.as_deref()),
            title,
            ids: MediaIds::default(),
            transport: transport(d.protocol.as_deref()),
            state,
            origin: Origin::Manager,
            progress: progress.unwrap_or(0.0),
            size_bytes: size,
            left_bytes: left,
            eta: time_left(d.time_left.as_deref()).and_then(|s| eta_in(round.now, s)),
        },
        download_id,
    })
}

fn recent_item(e: &HistoryEvent, round: &Round) -> Option<RecentItem> {
    // The query asks for imports only; a bindery that ignored the filter
    // must not turn grabs and failures into "recently added".
    if text(&e.event_type) != Some(IMPORT_EVENT) {
        return None;
    }
    let imported_at = parse_time(e.created_at.as_deref())?;
    if imported_at < round.recent_since {
        return None;
    }
    let title = book_title(e.book.as_ref()).or_else(|| client_title(e.source_title.as_deref()))?;
    Some(RecentItem {
        kind: media_kind(e.source_title.as_deref()),
        title,
        ids: MediaIds::default(),
        imported_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_states_map_to_classes() {
        for (status, state) in [
            ("grabbed", ItemState::Queued),
            ("downloading", ItemState::Downloading),
            ("completed", ItemState::Importing),
            ("importPending", ItemState::Importing),
            ("importing", ItemState::Importing),
            ("importExternal", ItemState::Importing),
            ("importHeld", ItemState::Importing),
            ("importFailed", ItemState::Warning),
            ("importBlocked", ItemState::Warning),
            ("failed", ItemState::Failed),
            ("imported", ItemState::Completed),
            ("something-new", ItemState::Queued),
        ] {
            assert_eq!(queue_state(Some(status)), state, "{status}");
        }
    }

    #[test]
    fn audio_tokens_make_an_audiobook_and_only_as_words() {
        for (release, kind) in [
            (
                "Stephen Fry - Mythos (Unabridged) M4B",
                MediaKind::Audiobook,
            ),
            ("Author - Title [MP3 64kbps]", MediaKind::Audiobook),
            ("Author.Title.2021.m4b+pdf", MediaKind::Audiobook),
            (
                "Andy.Weir-Project.Hail.Mary.2021.RETAIL.EPUB",
                MediaKind::Book,
            ),
            ("Author - Title (mp3player edition) epub", MediaKind::Book),
            ("No format at all", MediaKind::Book),
        ] {
            assert_eq!(media_kind(Some(release)), kind, "{release}");
        }
        assert_eq!(media_kind(None), MediaKind::Book);
    }

    #[test]
    fn time_left_reads_both_spellings() {
        assert_eq!(time_left(Some("1h 05m")), Some(3900.0));
        assert_eq!(time_left(Some("3m 07s")), Some(187.0));
        assert_eq!(time_left(Some("45s")), Some(45.0));
        assert_eq!(time_left(Some("0:12:34")), Some(754.0));
        assert_eq!(time_left(Some("1:00:00:00")), Some(86400.0));
        assert_eq!(time_left(Some("0:00:00")), None);
        assert_eq!(time_left(Some("")), None);
        assert_eq!(time_left(Some("soon")), None);
        assert_eq!(time_left(Some("5x")), None);
        assert_eq!(time_left(None), None);
    }

    #[test]
    fn percentages_are_fractions() {
        assert_eq!(percentage(Some("42.0")), Some(0.42));
        assert_eq!(percentage(Some("50")), Some(0.5));
        assert_eq!(percentage(Some("100%")), Some(1.0));
        assert_eq!(percentage(Some("")), None);
        assert_eq!(percentage(Some("NaN")), None);
    }
}
