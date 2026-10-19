//! Radarr, Sonarr and Lidarr: queue, calendar and recent imports.
//!
//! The three share their API shape (Lidarr on `/api/v1`, the others on
//! `/api/v3`) but not their nouns, so the structs below are a union of the
//! three, every field optional. Field names are from the OpenAPI files of
//! Radarr 6.4.4.10685, Sonarr 4.0.20.3014 and Lidarr 3.1.0.4875.
//!
//! What is NOT read here, deliberately: `errorMessage`, `statusMessages`,
//! `outputPath`, `indexer`, `downloadClient`, `path` — free text and names
//! that carry tracker URLs and passkeys.

use reqwest::Client;
use serde::Deserialize;
use url::Url;

use super::http::{self, FetchError};
use super::merge::{Contribution, Tracked};
use super::title::client_title;
use super::{Round, bytes, fraction, parse_time};
use crate::snapshot::{
    CalendarItem, ItemState, MediaIds, MediaKind, Origin, QueueItem, RecentItem, SourceKind,
    Transport,
};

type Query = &'static [(&'static str, &'static str)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrKind {
    Radarr,
    Sonarr,
    Lidarr,
}

impl ArrKind {
    fn api(self) -> &'static str {
        match self {
            ArrKind::Radarr | ArrKind::Sonarr => "/api/v3",
            ArrKind::Lidarr => "/api/v1",
        }
    }
}

pub struct Arr {
    kind: ArrKind,
    pub label: String,
    base: Url,
    key: String,
    http: Client,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct QueuePage {
    total_records: Option<i64>,
    records: Vec<QueueRecord>,
}

/// Entries per queue page. The managers' own UI asks for far fewer; a
/// thousand makes one page the normal case.
const QUEUE_PAGE_SIZE: usize = 1000;

/// Pages followed at most. Ten thousand queue entries is a backlog no person
/// reads on a status page, and every page is another request every round —
/// a bound keeps a runaway queue from making the collector the manager's
/// busiest client.
const QUEUE_MAX_PAGES: usize = 10;

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct QueueRecord {
    movie: Option<Movie>,
    series: Option<Series>,
    episode: Option<Episode>,
    artist: Option<Artist>,
    album: Option<Album>,
    season_number: Option<i64>,
    /// The release name: only a fallback when the manager has no title of
    /// its own for the entry.
    title: Option<String>,
    size: Option<f64>,
    sizeleft: Option<f64>,
    estimated_completion_time: Option<String>,
    status: Option<String>,
    tracked_download_status: Option<String>,
    tracked_download_state: Option<String>,
    download_id: Option<String>,
    protocol: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Movie {
    title: Option<String>,
    year: Option<i64>,
    tmdb_id: Option<i64>,
    in_cinemas: Option<String>,
    physical_release: Option<String>,
    digital_release: Option<String>,
    has_file: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Series {
    title: Option<String>,
    tvdb_id: Option<i64>,
    tmdb_id: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Episode {
    series: Option<Series>,
    season_number: Option<i64>,
    episode_number: Option<i64>,
    title: Option<String>,
    air_date_utc: Option<String>,
    has_file: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Artist {
    artist_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Album {
    title: Option<String>,
    foreign_album_id: Option<String>,
    release_date: Option<String>,
    artist: Option<Artist>,
    statistics: Option<AlbumStatistics>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AlbumStatistics {
    track_file_count: Option<i64>,
    track_count: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct HistoryRecord {
    date: Option<String>,
    movie: Option<Movie>,
    series: Option<Series>,
    episode: Option<Episode>,
    artist: Option<Artist>,
    album: Option<Album>,
}

fn id(value: Option<i64>) -> Option<u64> {
    value.filter(|v| *v > 0).map(|v| v as u64)
}

fn text(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn movie_title(m: &Movie) -> Option<String> {
    let title = text(&m.title)?;
    Some(match m.year.filter(|y| *y > 0) {
        Some(year) => format!("{title} ({year})"),
        None => title.to_string(),
    })
}

/// "Severance – S02E03 – Who Is Alive?". Sonarr names an unknown episode
/// "TBA"; that is left out rather than shown.
fn episode_title(
    series: Option<&Series>,
    episode: Option<&Episode>,
    season: Option<i64>,
) -> Option<String> {
    let series_title = text(&series?.title)?;
    let Some(e) = episode else {
        return Some(match season {
            Some(s) => format!("{series_title} – S{s:02}"),
            None => series_title.to_string(),
        });
    };
    let mut title = match (e.season_number, e.episode_number) {
        (Some(s), Some(n)) => format!("{series_title} – S{s:02}E{n:02}"),
        _ => series_title.to_string(),
    };
    if let Some(name) = text(&e.title).filter(|t| *t != "TBA") {
        title.push_str(" – ");
        title.push_str(name);
    }
    Some(title)
}

fn album_title(artist: Option<&Artist>, album: &Album) -> Option<String> {
    let name = text(&album.title)?;
    let artist = artist
        .or(album.artist.as_ref())
        .and_then(|a| text(&a.artist_name));
    Some(match artist {
        Some(a) => format!("{a} – {name}"),
        None => name.to_string(),
    })
}

fn series_ids(series: Option<&Series>) -> MediaIds {
    MediaIds {
        tvdb: series.and_then(|s| id(s.tvdb_id)),
        tmdb: series.and_then(|s| id(s.tmdb_id)),
        musicbrainz: None,
    }
}

/// The queue state, from the most to the least specific field. The tracked
/// state says where the manager is in its own process; the queue status says
/// what the client reported; the tracked status flags trouble.
fn queue_state(r: &QueueRecord) -> ItemState {
    let tracked = r.tracked_download_state.as_deref().unwrap_or("");
    match tracked {
        "importPending" | "importing" => return ItemState::Importing,
        "importBlocked" | "importFailed" | "ignored" => return ItemState::Warning,
        "failedPending" | "failed" | "downloadFailed" | "downloadFailedPending" => {
            return ItemState::Failed;
        }
        "imported" => return ItemState::Completed,
        _ => {}
    }
    let flag = r.tracked_download_status.as_deref().unwrap_or("");
    if flag.eq_ignore_ascii_case("error") {
        return ItemState::Failed;
    }
    // Lidarr's status is a plain string; compare without case.
    let status = r.status.as_deref().unwrap_or("").to_ascii_lowercase();
    let state = match status.as_str() {
        "downloading" => ItemState::Downloading,
        "paused" => ItemState::Paused,
        "completed" => ItemState::Importing,
        "failed" => ItemState::Failed,
        "warning" | "downloadclientunavailable" => ItemState::Warning,
        _ => ItemState::Queued,
    };
    if state == ItemState::Downloading && flag.eq_ignore_ascii_case("warning") {
        ItemState::Warning
    } else {
        state
    }
}

fn transport(protocol: Option<&str>) -> Transport {
    match protocol {
        Some("usenet") => Transport::Usenet,
        Some("torrent") => Transport::Torrent,
        _ => Transport::Unknown,
    }
}

impl Arr {
    pub fn new(
        kind: ArrKind,
        label: &str,
        url: &str,
        key: String,
        http: Client,
    ) -> anyhow::Result<Self> {
        Ok(Arr {
            kind,
            label: label.to_string(),
            base: http::base_url(url)?,
            key,
            http,
        })
    }

    pub fn kind(&self) -> SourceKind {
        match self.kind {
            ArrKind::Radarr => SourceKind::Radarr,
            ArrKind::Sonarr => SourceKind::Sonarr,
            ArrKind::Lidarr => SourceKind::Lidarr,
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, FetchError> {
        let url = http::endpoint(&self.base, &format!("{}{path}", self.kind.api()), query);
        http::get_json(self.http.get(url).header("X-Api-Key", &self.key)).await
    }

    /// The whole queue, page by page. Reading only the first page would
    /// leave a long queue silently cut off — the entries that are missing
    /// are the ones nobody is told about. `totalRecords` says when to stop;
    /// an empty page or [`QUEUE_MAX_PAGES`] stops it too, so that a count
    /// that does not add up cannot loop.
    async fn queue(&self, include: Query) -> Result<Vec<QueueRecord>, FetchError> {
        let size = QUEUE_PAGE_SIZE.to_string();
        let mut records = Vec::new();
        for page in 1..=QUEUE_MAX_PAGES {
            let number = page.to_string();
            let mut query = vec![("page", number.as_str()), ("pageSize", size.as_str())];
            query.extend_from_slice(include);
            let answer: QueuePage = self.get("/queue", &query).await?;
            let got = answer.records.len();
            records.extend(answer.records);
            let total = answer.total_records.unwrap_or(0).max(0) as usize;
            if got == 0 || records.len() >= total {
                return Ok(records);
            }
        }
        eprintln!(
            "queuewatch: {:?} {:?}: the queue has more than {} pages; the rest is left out",
            self.kind, self.label, QUEUE_MAX_PAGES
        );
        Ok(records)
    }

    pub async fn fetch(&self, round: &Round) -> Result<Contribution, FetchError> {
        let (queue_query, calendar_include, history_query): (Query, Query, Query) = match self.kind
        {
            ArrKind::Radarr => (
                &[("includeMovie", "true")],
                &[],
                &[
                    ("eventType", "downloadFolderImported"),
                    ("includeMovie", "true"),
                ],
            ),
            ArrKind::Sonarr => (
                &[("includeSeries", "true"), ("includeEpisode", "true")],
                &[("includeSeries", "true")],
                &[
                    ("eventType", "downloadFolderImported"),
                    ("includeSeries", "true"),
                    ("includeEpisode", "true"),
                ],
            ),
            // Lidarr records one `downloadImported` per album download
            // and a `trackFileImported` per track; the first is the one a
            // person means by "it arrived".
            ArrKind::Lidarr => (
                &[("includeArtist", "true"), ("includeAlbum", "true")],
                &[("includeArtist", "true")],
                &[
                    ("eventType", "downloadImported"),
                    ("includeArtist", "true"),
                    ("includeAlbum", "true"),
                ],
            ),
        };
        let start = round.calendar_start.to_string();
        let end = round.calendar_end.to_string();
        let mut calendar_query = vec![("start", start.as_str()), ("end", end.as_str())];
        calendar_query.extend_from_slice(calendar_include);
        let since = round.recent_since.to_string();
        let mut history = vec![("date", since.as_str())];
        history.extend_from_slice(history_query);

        let (queue, calendar, recent) = tokio::try_join!(
            self.queue(queue_query),
            self.get::<Vec<serde_json::Value>>("/calendar", &calendar_query),
            self.get::<Vec<HistoryRecord>>("/history/since", &history),
        )?;

        let mut part = Contribution::default();
        for r in &queue {
            if let Some(item) = self.queue_item(r) {
                part.managed.push(item);
            }
        }
        // The calendar arrives as raw values and is decoded entry by entry:
        // one entry with an odd field should cost that entry, not the round.
        for entry in calendar {
            if let Some(item) = self.calendar_item(entry, round) {
                part.calendar.push(item);
            }
        }
        for h in &recent {
            if let Some(item) = self.recent_item(h) {
                part.recent.push(item);
            }
        }
        Ok(part)
    }

    fn queue_item(&self, r: &QueueRecord) -> Option<Tracked> {
        let (kind, title, ids) = match self.kind {
            ArrKind::Radarr => (
                MediaKind::Movie,
                r.movie.as_ref().and_then(movie_title),
                MediaIds {
                    tmdb: r.movie.as_ref().and_then(|m| id(m.tmdb_id)),
                    ..Default::default()
                },
            ),
            ArrKind::Sonarr => {
                let series = r
                    .series
                    .as_ref()
                    .or(r.episode.as_ref().and_then(|e| e.series.as_ref()));
                (
                    MediaKind::Episode,
                    episode_title(series, r.episode.as_ref(), r.season_number),
                    series_ids(series),
                )
            }
            ArrKind::Lidarr => (
                MediaKind::Album,
                r.album
                    .as_ref()
                    .and_then(|a| album_title(r.artist.as_ref(), a)),
                MediaIds {
                    musicbrainz: r
                        .album
                        .as_ref()
                        .and_then(|a| text(&a.foreign_album_id).map(String::from)),
                    ..Default::default()
                },
            ),
        };
        // The release name is the fallback for an entry the manager could
        // not match (no movie, no series) — and for a grab by URL it is the
        // URL. It goes through the same filter as a client's names.
        let title = title.or_else(|| client_title(r.title.as_deref()))?;
        let size = r.size.unwrap_or(0.0);
        let left = r.sizeleft.unwrap_or(0.0);
        Some(Tracked {
            item: QueueItem {
                kind,
                title,
                ids,
                transport: transport(r.protocol.as_deref()),
                state: queue_state(r),
                origin: Origin::Manager,
                progress: fraction(size - left, size),
                size_bytes: bytes(r.size).filter(|b| *b > 0),
                left_bytes: bytes(r.sizeleft),
                eta: parse_time(r.estimated_completion_time.as_deref()),
            },
            download_id: text(&r.download_id).map(String::from),
        })
    }

    fn calendar_item(&self, entry: serde_json::Value, round: &Round) -> Option<CalendarItem> {
        match self.kind {
            ArrKind::Radarr => {
                let m: Movie = serde_json::from_value(entry).ok()?;
                // A film has up to three release dates; the one that matters
                // is the first that falls into the window.
                let airs_at = [&m.in_cinemas, &m.digital_release, &m.physical_release]
                    .into_iter()
                    .filter_map(|d| parse_time(d.as_deref()))
                    .filter(|t| *t >= round.calendar_start && *t <= round.calendar_end)
                    .min()?;
                Some(CalendarItem {
                    kind: MediaKind::Movie,
                    title: movie_title(&m)?,
                    ids: MediaIds {
                        tmdb: id(m.tmdb_id),
                        ..Default::default()
                    },
                    airs_at,
                    has_file: m.has_file.unwrap_or(false),
                })
            }
            ArrKind::Sonarr => {
                let e: Episode = serde_json::from_value(entry).ok()?;
                Some(CalendarItem {
                    kind: MediaKind::Episode,
                    title: episode_title(e.series.as_ref(), Some(&e), None)?,
                    ids: series_ids(e.series.as_ref()),
                    airs_at: parse_time(e.air_date_utc.as_deref())?,
                    has_file: e.has_file.unwrap_or(false),
                })
            }
            ArrKind::Lidarr => {
                let a: Album = serde_json::from_value(entry).ok()?;
                let has_file = a.statistics.as_ref().is_some_and(|s| {
                    s.track_count.unwrap_or(0) > 0 && s.track_file_count >= s.track_count
                });
                Some(CalendarItem {
                    kind: MediaKind::Album,
                    title: album_title(None, &a)?,
                    ids: MediaIds {
                        musicbrainz: text(&a.foreign_album_id).map(String::from),
                        ..Default::default()
                    },
                    airs_at: parse_time(a.release_date.as_deref())?,
                    has_file,
                })
            }
        }
    }

    fn recent_item(&self, h: &HistoryRecord) -> Option<RecentItem> {
        let imported_at = parse_time(h.date.as_deref())?;
        let (kind, title, ids) = match self.kind {
            ArrKind::Radarr => {
                let m = h.movie.as_ref()?;
                (
                    MediaKind::Movie,
                    movie_title(m)?,
                    MediaIds {
                        tmdb: id(m.tmdb_id),
                        ..Default::default()
                    },
                )
            }
            ArrKind::Sonarr => {
                let series = h
                    .series
                    .as_ref()
                    .or(h.episode.as_ref().and_then(|e| e.series.as_ref()));
                (
                    MediaKind::Episode,
                    episode_title(series, h.episode.as_ref(), None)?,
                    series_ids(series),
                )
            }
            ArrKind::Lidarr => {
                let a = h.album.as_ref()?;
                (
                    MediaKind::Album,
                    album_title(h.artist.as_ref(), a)?,
                    MediaIds {
                        musicbrainz: text(&a.foreign_album_id).map(String::from),
                        ..Default::default()
                    },
                )
            }
        };
        Some(RecentItem {
            kind,
            title,
            ids,
            imported_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: &str, tracked_status: &str, tracked_state: &str) -> QueueRecord {
        QueueRecord {
            status: Some(status.into()),
            tracked_download_status: Some(tracked_status.into()),
            tracked_download_state: Some(tracked_state.into()),
            ..Default::default()
        }
    }

    #[test]
    fn queue_states_map_to_classes() {
        assert_eq!(
            queue_state(&record("downloading", "ok", "downloading")),
            ItemState::Downloading
        );
        assert_eq!(
            queue_state(&record("downloading", "warning", "downloading")),
            ItemState::Warning
        );
        assert_eq!(
            queue_state(&record("completed", "warning", "importBlocked")),
            ItemState::Warning
        );
        assert_eq!(
            queue_state(&record("completed", "ok", "importPending")),
            ItemState::Importing
        );
        assert_eq!(
            queue_state(&record("failed", "error", "failedPending")),
            ItemState::Failed
        );
        assert_eq!(
            queue_state(&record("delay", "ok", "downloading")),
            ItemState::Queued
        );
        assert_eq!(
            queue_state(&record("Paused", "ok", "downloading")),
            ItemState::Paused
        );
    }

    #[test]
    fn episode_titles_leave_out_tba() {
        let s = Series {
            title: Some("Severance".into()),
            ..Default::default()
        };
        let mut e = Episode {
            season_number: Some(2),
            episode_number: Some(3),
            title: Some("Who Is Alive?".into()),
            ..Default::default()
        };
        assert_eq!(
            episode_title(Some(&s), Some(&e), None).unwrap(),
            "Severance – S02E03 – Who Is Alive?"
        );
        e.title = Some("TBA".into());
        assert_eq!(
            episode_title(Some(&s), Some(&e), None).unwrap(),
            "Severance – S02E03"
        );
        assert_eq!(
            episode_title(Some(&s), None, Some(1)).unwrap(),
            "Severance – S01"
        );
    }
}
