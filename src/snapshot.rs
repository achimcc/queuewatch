//! The snapshot: everything the collector learned in one round, and the only
//! thing that crosses from `collect` to `serve`.
//!
//! This type is the security boundary of the whole design. The collector
//! holds API keys that can delete a library; the page that people reach from
//! the internet holds none. What it may show is exactly what fits in these
//! structs, so they are built from a **closed list of fields**:
//!
//! - no free-text error or status message from any service — those carry
//!   tracker announce URLs, and with them passkeys. A problem becomes a
//!   [`ItemState`] class instead;
//! - the one exception is the release or file name of a download no manager
//!   names ([`QueueItem::title`]), and it passes a filter first: a client
//!   shows a URL there while it has not fetched the job yet, key included;
//! - no paths, no host names, no indexer or tracker names, no download client
//!   names;
//! - no keys, tokens or cookies, obviously.
//!
//! Whoever adds a field here adds it to what strangers on the internet could
//! read if the sign-in failed open. That is the question to ask first.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// The title of a queue item whose name the collector would not pass on
/// (see `collect::title`). A fixed English marker rather than an empty string:
/// an empty title reads as a rendering bug, and `serve` swaps this exact
/// value for its own words in the page's language.
pub const UNNAMED: &str = "(unnamed download)";

/// Bumped on any change that an older `serve` could misread. `serve` refuses
/// a snapshot whose version it does not know rather than guessing.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub taken_at: Timestamp,
    /// One entry per configured instance, in configuration order — including
    /// the ones that did not answer, so the page can say so.
    pub sources: Vec<SourceStatus>,
    /// Everything that is downloading, waiting or being imported, merged
    /// across the managers and the download clients (see [`QueueItem::origin`]).
    pub queue: Vec<QueueItem>,
    pub indexers: IndexerSummary,
    /// Releases in the coming days, oldest first.
    pub calendar: Vec<CalendarItem>,
    /// Imported recently, newest first.
    pub recent: Vec<RecentItem>,
    /// Requests from the request manager (Seerr), newest first.
    pub requests: Vec<Request>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Radarr,
    Sonarr,
    Lidarr,
    Prowlarr,
    Sabnzbd,
    Qbittorrent,
    Pyload,
    Slskd,
    Seerr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub kind: SourceKind,
    /// The label from the configuration ("Movies", "Series"), not a host name.
    pub label: String,
    pub available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Movie,
    Episode,
    Album,
    /// A download no manager claims (a pyLoad link, a manual torrent).
    Other,
}

/// How a download travels. Says nothing about which client or which tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Usenet,
    Torrent,
    Hoster,
    Soulseek,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemState {
    Queued,
    Downloading,
    Paused,
    /// Finished downloading, the manager is moving it into the library.
    Importing,
    /// No progress for a while: no peers, no source.
    Stalled,
    /// The manager needs a human (wrong match, unpack problem, …).
    Warning,
    Failed,
    Completed,
}

/// Which side reported an item. A manager's queue entry and the download
/// client's entry for the same download are merged into one item with origin
/// `Manager`; a client entry no manager claims stays as `Client`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Manager,
    Client,
}

/// Identifiers that tie a queue item, a calendar entry or a request to the
/// same title. Each is optional: Lidarr knows MusicBrainz, not TMDB.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tvdb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub musicbrainz: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub kind: MediaKind,
    /// For people, not for machines: "Dune: Part Two (2024)",
    /// "Severance – S02E03 – Who Is Alive?", "Björk – Fossora". For a client
    /// item no manager claims, the release name as the client shows it —
    /// the one piece of service-provided text in the snapshot, and only after
    /// `collect::title::client_title` has turned anything that looks like a
    /// URL, a key or a path into [`UNNAMED`].
    pub title: String,
    pub ids: MediaIds,
    pub transport: Transport,
    pub state: ItemState,
    pub origin: Origin,
    /// 0.0 to 1.0.
    pub progress: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta: Option<Timestamp>,
}

/// Indexers, counted and never named: a private tracker's name is the thing
/// its members are asked not to spread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerSummary {
    /// Enabled indexers.
    pub enabled: u32,
    /// Enabled and not currently failing or backed off.
    pub healthy: u32,
    /// False when the indexer manager did not answer — the counts are then 0
    /// and mean nothing.
    pub known: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarItem {
    pub kind: MediaKind,
    pub title: String,
    pub ids: MediaIds,
    pub airs_at: Timestamp,
    /// Already in the library (an episode that aired and was grabbed).
    pub has_file: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentItem {
    pub kind: MediaKind,
    pub title: String,
    pub ids: MediaIds,
    pub imported_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestState {
    /// Waiting for an admin to approve.
    Pending,
    Declined,
    /// Approved, not yet in the library.
    Processing,
    /// Some seasons or episodes are there.
    Partial,
    Available,
    Failed,
}

/// The person behind a request, as the request manager knows them. Matched
/// against the signed-in identity by `serve`; never shown to anyone else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requester {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub kind: MediaKind,
    pub title: String,
    pub ids: MediaIds,
    pub requested_by: Requester,
    pub requested_at: Timestamp,
    pub state: RequestState,
    /// Progress of the matching queue items, joined by the collector over
    /// [`MediaIds`]: 0.0 to 1.0, `None` when nothing is downloading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta: Option<Timestamp>,
}
