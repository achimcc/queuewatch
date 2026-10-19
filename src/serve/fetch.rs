//! Getting the snapshot from the collector.
//!
//! The page never talks to a service; it asks the collector, and only for
//! the one document [`Snapshot`]. A failure here is never an error page:
//! people still get the page, told that it is not current, with the last
//! good snapshot if there was one.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{AUTHORIZATION, HeaderValue};

use crate::snapshot::{SCHEMA_VERSION, Snapshot};

/// A snapshot is a few hundred kilobytes at most; anything beyond this is not
/// a snapshot, and reading it would only cost memory.
const MAX_BODY: usize = 16 * 1024 * 1024;

pub struct SnapshotSource {
    url: String,
    /// Marked sensitive, so that it does not appear in any `Debug` output
    /// of a request — the token is never logged.
    auth: HeaderValue,
    http: reqwest::Client,
    max_age: Duration,
    cache: tokio::sync::Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    good: Option<Arc<Snapshot>>,
    good_at: Option<Instant>,
    /// The last failed attempt. While it is recent, the collector is not
    /// asked again: a collector that is down would otherwise cost every page
    /// view a full timeout.
    failed_at: Option<Instant>,
}

/// What the page gets.
#[derive(Clone)]
pub struct View {
    /// The newest good snapshot, current or not.
    pub snapshot: Option<Arc<Snapshot>>,
    /// False when the last attempt to reach the collector failed.
    pub reachable: bool,
}

impl SnapshotSource {
    pub fn new(url: String, token: &str, max_age: Duration) -> anyhow::Result<Self> {
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("the snapshot token is not a valid header value"))?;
        auth.set_sensitive(true);
        let http = reqwest::Client::builder()
            // The collector answers itself; a redirect is not something it
            // does, and following one would carry the token elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            url,
            auth,
            http,
            max_age,
            cache: tokio::sync::Mutex::new(Cache::default()),
        })
    }

    /// The snapshot to render, fetched if the cached one is older than
    /// `max_age`. One fetch at a time: concurrent page views wait for it
    /// rather than each asking the collector.
    pub async fn get(&self) -> View {
        let mut cache = self.cache.lock().await;
        let fresh = |at: Option<Instant>| at.is_some_and(|t| t.elapsed() < self.max_age);

        if let Some(good) = &cache.good
            && fresh(cache.good_at)
            && cache.failed_at.is_none()
        {
            return View {
                snapshot: Some(good.clone()),
                reachable: true,
            };
        }
        if fresh(cache.failed_at) {
            return View {
                snapshot: cache.good.clone(),
                reachable: false,
            };
        }

        match self.fetch().await {
            Ok(snapshot) => {
                let snapshot = Arc::new(snapshot);
                cache.good = Some(snapshot.clone());
                cache.good_at = Some(Instant::now());
                cache.failed_at = None;
                View {
                    snapshot: Some(snapshot),
                    reachable: true,
                }
            }
            Err(e) => {
                // The error names the URL and the status, never the header:
                // reqwest does not print request headers in its errors.
                eprintln!("queuewatch: cannot get the snapshot: {e:#}");
                cache.failed_at = Some(Instant::now());
                View {
                    snapshot: cache.good.clone(),
                    reachable: false,
                }
            }
        }
    }

    async fn fetch(&self) -> anyhow::Result<Snapshot> {
        let mut response = self
            .http
            .get(&self.url)
            .header(AUTHORIZATION, self.auth.clone())
            .send()
            .await?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            anyhow::bail!("the collector answered {status}");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > MAX_BODY {
                anyhow::bail!("the snapshot is larger than {MAX_BODY} bytes");
            }
            body.extend_from_slice(&chunk);
        }
        parse(&body)
    }
}

/// The version is read before the rest: a snapshot of another version may
/// still parse — a field renamed with a default in place — and mean
/// something else. It is refused, not guessed at.
pub fn parse(body: &[u8]) -> anyhow::Result<Snapshot> {
    let value: serde_json::Value = serde_json::from_slice(body)?;
    match value.get("version").and_then(|v| v.as_u64()) {
        Some(v) if v == u64::from(SCHEMA_VERSION) => {}
        Some(v) => anyhow::bail!(
            "the collector sends snapshot version {v}, this page understands {SCHEMA_VERSION}"
        ),
        None => anyhow::bail!("the collector's answer has no snapshot version"),
    }
    Ok(serde_json::from_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_different_version_is_refused_even_when_it_would_parse() {
        let body = br#"{"version":2,"taken_at":"2026-10-19T08:00:00Z","sources":[],"queue":[],
            "indexers":{"enabled":0,"healthy":0,"known":false},"calendar":[],"recent":[],"requests":[]}"#;
        let err = parse(body).expect_err("must refuse");
        assert!(err.to_string().contains("version 2"), "{err}");
        let ok = String::from_utf8_lossy(body).replace("\"version\":2", "\"version\":1");
        assert!(parse(ok.as_bytes()).is_ok());
        assert!(parse(b"{}").is_err());
    }
}
