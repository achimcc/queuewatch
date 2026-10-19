//! The collector: the half that holds the API keys.
//!
//! Every `interval_secs` it asks every configured service what it is doing,
//! all at once, and turns the answers into a [`Snapshot`]. The snapshot is
//! the only thing it hands out, over `GET /snapshot`, and only to whoever
//! presents the bearer token. A service that does not answer is marked
//! unavailable; it never takes the others down with it.
//!
//! Each source module reads its service's own JSON into structs that name
//! only the fields the snapshot needs. That is the second fence behind
//! [`crate::snapshot`]: an error message, a path or a tracker URL is never
//! even deserialized, so no later code can copy it by accident.

pub mod arr;
pub mod bindery;
pub mod http;
pub mod merge;
pub mod prowlarr;
pub mod pyload;
pub mod qbittorrent;
pub mod sabnzbd;
pub mod seerr;
pub mod slskd;
pub mod title;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use jiff::{SignedDuration, Timestamp};
use tokio::task::JoinSet;

use crate::config::{CollectConfig, read_secret};
use crate::snapshot::{Snapshot, SourceKind, SourceStatus};
pub use http::FetchError;
pub use merge::Contribution;

/// The time frame of one round, computed once so that every source asks for
/// the same window.
#[derive(Debug, Clone, Copy)]
pub struct Round {
    pub now: Timestamp,
    pub calendar_start: Timestamp,
    pub calendar_end: Timestamp,
    pub recent_since: Timestamp,
}

impl Round {
    pub fn new(now: Timestamp, calendar_days: u32, recent_hours: u32) -> Self {
        // From the start of today, so that an episode that aired this morning
        // and is already in the library still shows, with `has_file`.
        let calendar_start = now
            .to_zoned(jiff::tz::TimeZone::UTC)
            .start_of_day()
            .map(|z| z.timestamp())
            .unwrap_or(now);
        Round {
            now,
            calendar_start,
            calendar_end: now + SignedDuration::from_hours(i64::from(calendar_days) * 24),
            recent_since: now - SignedDuration::from_hours(i64::from(recent_hours)),
        }
    }
}

/// One configured instance.
pub enum Source {
    Arr(arr::Arr),
    Prowlarr(prowlarr::Prowlarr),
    Sabnzbd(sabnzbd::Sabnzbd),
    Qbittorrent(qbittorrent::Qbittorrent),
    Pyload(pyload::Pyload),
    Slskd(slskd::Slskd),
    Seerr(seerr::Seerr),
    Bindery(bindery::Bindery),
}

impl Source {
    pub fn kind(&self) -> SourceKind {
        match self {
            Source::Arr(s) => s.kind(),
            Source::Prowlarr(_) => SourceKind::Prowlarr,
            Source::Sabnzbd(_) => SourceKind::Sabnzbd,
            Source::Qbittorrent(_) => SourceKind::Qbittorrent,
            Source::Pyload(_) => SourceKind::Pyload,
            Source::Slskd(_) => SourceKind::Slskd,
            Source::Seerr(_) => SourceKind::Seerr,
            Source::Bindery(_) => SourceKind::Bindery,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Source::Arr(s) => &s.label,
            Source::Prowlarr(s) => &s.label,
            Source::Sabnzbd(s) => &s.label,
            Source::Qbittorrent(s) => &s.label,
            Source::Pyload(s) => &s.label,
            Source::Slskd(s) => &s.label,
            Source::Seerr(s) => &s.label,
            Source::Bindery(s) => &s.label,
        }
    }

    pub async fn fetch(&self, round: &Round) -> Result<Contribution, FetchError> {
        match self {
            Source::Arr(s) => s.fetch(round).await,
            Source::Prowlarr(s) => s.fetch().await,
            Source::Sabnzbd(s) => s.fetch(round).await,
            Source::Qbittorrent(s) => s.fetch(round).await,
            Source::Pyload(s) => s.fetch(round).await,
            Source::Slskd(s) => s.fetch(round).await,
            Source::Seerr(s) => s.fetch().await,
            Source::Bindery(s) => s.fetch(round).await,
        }
    }
}

fn kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Radarr => "radarr",
        SourceKind::Sonarr => "sonarr",
        SourceKind::Lidarr => "lidarr",
        SourceKind::Prowlarr => "prowlarr",
        SourceKind::Sabnzbd => "sabnzbd",
        SourceKind::Qbittorrent => "qbittorrent",
        SourceKind::Pyload => "pyload",
        SourceKind::Slskd => "slskd",
        SourceKind::Seerr => "seerr",
        SourceKind::Bindery => "bindery",
    }
}

pub struct Collector {
    sources: Vec<Arc<Source>>,
    calendar_days: u32,
    recent_hours: u32,
}

impl Collector {
    pub fn new(sources: Vec<Source>, calendar_days: u32, recent_hours: u32) -> Self {
        Collector {
            sources: sources.into_iter().map(Arc::new).collect(),
            calendar_days,
            recent_hours,
        }
    }

    /// Reads every key file now, so that a missing credential stops the
    /// start and does not show up as "unavailable" forever after.
    pub fn from_config(config: &CollectConfig) -> anyhow::Result<Self> {
        let client = http::client(Duration::from_secs(config.timeout_secs))?;
        let mut sources = Vec::new();
        let arr_kinds = [
            (arr::ArrKind::Radarr, &config.radarr),
            (arr::ArrKind::Sonarr, &config.sonarr),
            (arr::ArrKind::Lidarr, &config.lidarr),
        ];
        for (kind, instances) in arr_kinds {
            for i in instances {
                let key = read_secret(&i.api_key_file)?;
                let s = arr::Arr::new(kind, &i.label, &i.url, key, client.clone())
                    .with_context(|| format!("{:?} {:?}", kind, i.label))?;
                sources.push(Source::Arr(s));
            }
        }
        for i in &config.prowlarr {
            let key = read_secret(&i.api_key_file)?;
            let s = prowlarr::Prowlarr::new(&i.label, &i.url, key, client.clone())
                .with_context(|| format!("prowlarr {:?}", i.label))?;
            sources.push(Source::Prowlarr(s));
        }
        for i in &config.sabnzbd {
            let key = read_secret(&i.api_key_file)?;
            let s = sabnzbd::Sabnzbd::new(&i.label, &i.url, key, client.clone())
                .with_context(|| format!("sabnzbd {:?}", i.label))?;
            sources.push(Source::Sabnzbd(s));
        }
        for i in &config.qbittorrent {
            let password = read_secret(&i.password_file)?;
            let s = qbittorrent::Qbittorrent::new(
                &i.label,
                &i.url,
                &i.username,
                password,
                client.clone(),
            )
            .with_context(|| format!("qbittorrent {:?}", i.label))?;
            sources.push(Source::Qbittorrent(s));
        }
        for i in &config.pyload {
            let password = read_secret(&i.password_file)?;
            let s = pyload::Pyload::new(&i.label, &i.url, &i.username, password, client.clone())
                .with_context(|| format!("pyload {:?}", i.label))?;
            sources.push(Source::Pyload(s));
        }
        for i in &config.slskd {
            let key = read_secret(&i.api_key_file)?;
            let s = slskd::Slskd::new(&i.label, &i.url, key, client.clone())
                .with_context(|| format!("slskd {:?}", i.label))?;
            sources.push(Source::Slskd(s));
        }
        for i in &config.seerr {
            let key = read_secret(&i.api_key_file)?;
            let s = seerr::Seerr::new(&i.label, &i.url, key, client.clone())
                .with_context(|| format!("seerr {:?}", i.label))?;
            sources.push(Source::Seerr(s));
        }
        for i in &config.bindery {
            let key = read_secret(&i.api_key_file)?;
            let s = bindery::Bindery::new(&i.label, &i.url, key, client.clone())
                .with_context(|| format!("bindery {:?}", i.label))?;
            sources.push(Source::Bindery(s));
        }
        Ok(Collector::new(
            sources,
            config.calendar_days,
            config.recent_hours,
        ))
    }

    /// One round: every source at once, then the merge.
    pub async fn round(&self) -> Snapshot {
        let now = Timestamp::now();
        let round = Round::new(now, self.calendar_days, self.recent_hours);
        let mut tasks = JoinSet::new();
        for (index, source) in self.sources.iter().enumerate() {
            let source = Arc::clone(source);
            tasks.spawn(async move { (index, source.fetch(&round).await) });
        }
        let mut results: Vec<Option<Result<Contribution, FetchError>>> =
            (0..self.sources.len()).map(|_| None).collect();
        while let Some(joined) = tasks.join_next().await {
            // A panicking source leaves its slot empty: unavailable, like any
            // other failure, and logged below.
            if let Ok((index, result)) = joined {
                results[index] = Some(result);
            }
        }
        let mut statuses = Vec::with_capacity(self.sources.len());
        let mut parts = Vec::new();
        for (source, result) in self.sources.iter().zip(results) {
            let available = match result {
                Some(Ok(part)) => {
                    parts.push(part);
                    true
                }
                Some(Err(e)) => {
                    eprintln!(
                        "queuewatch: {} {:?} unavailable: {e}",
                        kind_name(source.kind()),
                        source.label()
                    );
                    false
                }
                None => {
                    eprintln!(
                        "queuewatch: {} {:?} unavailable: panic",
                        kind_name(source.kind()),
                        source.label()
                    );
                    false
                }
            };
            statuses.push(SourceStatus {
                kind: source.kind(),
                label: source.label().to_string(),
                available,
            });
        }
        merge::assemble(now, statuses, parts)
    }
}

/// The latest snapshot, shared between the collecting loop and the endpoint.
#[derive(Clone, Default)]
pub struct Latest(Arc<RwLock<Option<Arc<Snapshot>>>>);

impl Latest {
    pub fn set(&self, snapshot: Snapshot) {
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(snapshot));
    }

    pub fn get(&self) -> Option<Arc<Snapshot>> {
        self.0.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[derive(Clone)]
struct AppState {
    latest: Latest,
    token: Arc<str>,
}

/// `GET /snapshot` behind the bearer token, `GET /healthz` without. Nothing
/// else: this listener faces only `serve`, and every extra route would be one
/// more thing reachable with the token.
pub fn router(latest: Latest, token: String) -> Router {
    Router::new()
        .route("/snapshot", get(snapshot))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(AppState {
            latest,
            token: token.into(),
        })
}

async fn snapshot(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(presented.as_bytes(), state.token.as_bytes()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.latest.get() {
        Some(snapshot) => axum::Json(snapshot.as_ref().clone()).into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Compares without stopping at the first difference, so the answer time
/// does not tell how much of a guess was right. The length is not secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn run(config: CollectConfig) -> anyhow::Result<()> {
    let token = read_secret(&config.token_file)?;
    let collector = Collector::from_config(&config)?;
    let latest = Latest::default();
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("cannot listen on {}", config.listen))?;

    let interval = Duration::from_secs(config.interval_secs.max(1));
    let loop_latest = latest.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // A slow round must not be followed by a burst of catch-up rounds.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            loop_latest.set(collector.round().await);
        }
    });

    axum::serve(listener, router(latest, token))
        .await
        .context("serving the snapshot")
}

/// Timestamps as the services write them: RFC 3339 mostly, sometimes without
/// an offset (then it is UTC — the *arr apps store UTC). Anything else is no
/// date, not a failed source.
pub(crate) fn parse_time(raw: Option<&str>) -> Option<Timestamp> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(ts) = raw.parse::<Timestamp>() {
        return Some(ts);
    }
    raw.parse::<jiff::civil::DateTime>()
        .ok()
        .and_then(|dt| dt.to_zoned(jiff::tz::TimeZone::UTC).ok())
        .map(|z| z.timestamp())
}

/// `done / total`, clamped to 0..=1; 0 when the total is unknown.
pub(crate) fn fraction(done: f64, total: f64) -> f32 {
    if total > 0.0 && done.is_finite() {
        (done / total).clamp(0.0, 1.0) as f32
    } else {
        0.0
    }
}

/// `now + seconds` for a positive, finite number of seconds.
pub(crate) fn eta_in(now: Timestamp, seconds: f64) -> Option<Timestamp> {
    if seconds.is_finite() && seconds > 0.0 && seconds < 1.0e9 {
        now.checked_add(SignedDuration::from_secs_f64(seconds)).ok()
    } else {
        None
    }
}

/// Non-negative byte counts from a JSON number that may be a float.
pub(crate) fn bytes(value: Option<f64>) -> Option<u64> {
    value
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_without_offset_are_utc_and_garbage_is_none() {
        assert_eq!(
            parse_time(Some("2026-10-20T00:00:00")),
            Some("2026-10-20T00:00:00Z".parse().unwrap())
        );
        assert_eq!(
            parse_time(Some("2026-10-20T01:02:03.456Z")),
            Some("2026-10-20T01:02:03.456Z".parse().unwrap())
        );
        assert_eq!(parse_time(Some("soon")), None);
        assert_eq!(parse_time(None), None);
    }

    #[test]
    fn the_token_comparison_needs_every_byte() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
