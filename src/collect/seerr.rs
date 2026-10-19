//! Seerr: who asked for what, and how far it got.
//!
//! `GET /api/v1/request` (Seerr 3.2.0, `seerr-api.yml` and
//! `server/entity/MediaRequest.ts` — the YAML lacks `type` and
//! `jellyfinUsername`, the entities have them). A request carries ids but
//! no title, so titles come from `/movie/{id}` and `/tv/{id}` and are kept
//! across rounds: a title does not change, and a hundred lookups every thirty
//! seconds would be Seerr's busiest client.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use url::Url;

use super::http::{self, FetchError};
use super::merge::Contribution;
use super::parse_time;
use crate::snapshot::{MediaIds, MediaKind, Request, RequestState, Requester};

/// Title lookups in flight at once on the first round.
const PARALLEL_LOOKUPS: usize = 8;

/// Requests per page, and pages at most. Seerr has no "all"; a household
/// with more than a thousand requests on record sees the newest thousand,
/// which is more than the page has room for anyway — the bound is there so
/// that one round costs Seerr ten requests, not a walk through its history.
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 10;

pub struct Seerr {
    pub label: String,
    base: Url,
    key: String,
    http: Client,
    titles: Mutex<HashMap<(bool, u64), String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Page {
    results: Vec<MediaRequest>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct MediaRequest {
    status: Option<i64>,
    #[serde(rename = "type")]
    kind: Option<String>,
    created_at: Option<String>,
    media: Option<Media>,
    requested_by: Option<User>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Media {
    tmdb_id: Option<i64>,
    tvdb_id: Option<i64>,
    status: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct User {
    username: Option<String>,
    jellyfin_username: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Details {
    title: Option<String>,
    name: Option<String>,
    release_date: Option<String>,
}

fn id(value: Option<i64>) -> Option<u64> {
    value.filter(|v| *v > 0).map(|v| v as u64)
}

fn some_text(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `MediaRequestStatus` (1 pending, 2 approved, 3 declined, 4 failed,
/// 5 completed) refined by `MediaStatus` (2 pending, 3 processing,
/// 4 partially available, 5 available) once the request is approved.
fn state(request: Option<i64>, media: Option<i64>) -> RequestState {
    match request {
        Some(1) => RequestState::Pending,
        Some(3) => RequestState::Declined,
        Some(4) => RequestState::Failed,
        _ => match media {
            Some(5) => RequestState::Available,
            Some(4) => RequestState::Partial,
            _ if request == Some(5) => RequestState::Available,
            _ => RequestState::Processing,
        },
    }
}

impl Seerr {
    pub fn new(label: &str, url: &str, key: String, http: Client) -> anyhow::Result<Self> {
        Ok(Seerr {
            label: label.to_string(),
            base: http::base_url(url)?,
            key,
            http,
            titles: Mutex::new(HashMap::new()),
        })
    }

    /// Every request, newest first, page by page until a page comes back
    /// short. Only the first page would make a person's older request
    /// vanish from "My requests" as soon as others asked for a hundred
    /// things after it.
    async fn requests(&self) -> Result<Vec<MediaRequest>, FetchError> {
        let take = PAGE_SIZE.to_string();
        let mut all = Vec::new();
        for page in 0..MAX_PAGES {
            let skip = (page * PAGE_SIZE).to_string();
            let url = http::endpoint(
                &self.base,
                "/api/v1/request",
                &[("take", &take), ("skip", &skip), ("sort", "added")],
            );
            let answer: Page =
                http::get_json(self.http.get(url).header("X-Api-Key", &self.key)).await?;
            let got = answer.results.len();
            all.extend(answer.results);
            if got < PAGE_SIZE {
                break;
            }
        }
        Ok(all)
    }

    pub async fn fetch(&self) -> Result<Contribution, FetchError> {
        let results = self.requests().await?;

        let wanted: Vec<(bool, u64)> = results
            .iter()
            .filter_map(|r| {
                Some((
                    r.kind.as_deref() == Some("tv"),
                    id(r.media.as_ref()?.tmdb_id)?,
                ))
            })
            .collect();
        self.look_up_titles(&wanted).await;
        let titles = self
            .titles
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();

        let requests = results
            .into_iter()
            .filter_map(|r| {
                let tv = match r.kind.as_deref() {
                    Some("tv") => true,
                    Some("movie") => false,
                    _ => return None,
                };
                let media = r.media.unwrap_or_default();
                let tmdb = id(media.tmdb_id);
                // A title that could not be looked up still leaves the
                // request visible; the id is all there is to call it.
                let title = tmdb
                    .and_then(|t| titles.get(&(tv, t)).cloned())
                    .unwrap_or_else(|| format!("TMDB {}", tmdb.unwrap_or(0)));
                let user = r.requested_by.unwrap_or_default();
                Some(Request {
                    kind: if tv {
                        MediaKind::Episode
                    } else {
                        MediaKind::Movie
                    },
                    title,
                    ids: MediaIds {
                        tmdb,
                        tvdb: id(media.tvdb_id),
                        musicbrainz: None,
                    },
                    requested_by: Requester {
                        username: some_text(user.jellyfin_username).or(some_text(user.username)),
                        email: some_text(user.email),
                    },
                    requested_at: parse_time(r.created_at.as_deref())?,
                    state: state(r.status, media.status),
                    progress: None,
                    eta: None,
                })
            })
            .collect();
        Ok(Contribution {
            requests,
            ..Default::default()
        })
    }

    /// Fills the title cache for the ids it does not hold yet. A failed
    /// lookup is retried next round; it is counted, not logged per title.
    async fn look_up_titles(&self, wanted: &[(bool, u64)]) {
        let missing: Vec<(bool, u64)> = {
            let titles = self.titles.lock().unwrap_or_else(|p| p.into_inner());
            let mut m: Vec<_> = wanted
                .iter()
                .filter(|k| !titles.contains_key(k))
                .copied()
                .collect();
            m.sort_unstable();
            m.dedup();
            m
        };
        if missing.is_empty() {
            return;
        }
        let limit = Arc::new(Semaphore::new(PARALLEL_LOOKUPS));
        let mut tasks = JoinSet::new();
        for (tv, tmdb) in missing {
            let path = if tv {
                format!("/api/v1/tv/{tmdb}")
            } else {
                format!("/api/v1/movie/{tmdb}")
            };
            let url = http::endpoint(&self.base, &path, &[]);
            let request = self.http.get(url).header("X-Api-Key", &self.key);
            let limit = Arc::clone(&limit);
            tasks.spawn(async move {
                let _permit = limit.acquire_owned().await;
                let details: Result<Details, FetchError> = http::get_json(request).await;
                ((tv, tmdb), details)
            });
        }
        let mut failed: Vec<FetchError> = Vec::new();
        while let Some(joined) = tasks.join_next().await {
            let Ok((key, details)) = joined else { continue };
            match details {
                Ok(d) => {
                    let title = if key.0 {
                        some_text(d.name)
                    } else {
                        some_text(d.title)
                    };
                    let Some(title) = title else { continue };
                    let year = d
                        .release_date
                        .as_deref()
                        .and_then(|r| r.get(..4))
                        .filter(|y| y.bytes().all(|b| b.is_ascii_digit()));
                    let title = match (key.0, year) {
                        (false, Some(y)) => format!("{title} ({y})"),
                        _ => title,
                    };
                    self.titles
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(key, title);
                }
                Err(e) => failed.push(e),
            }
        }
        if let Some(first) = failed.first() {
            eprintln!(
                "queuewatch: seerr {:?}: {} title lookups failed (first: {first})",
                self.label,
                failed.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_media_status_combine() {
        assert_eq!(state(Some(1), Some(2)), RequestState::Pending);
        assert_eq!(state(Some(2), Some(3)), RequestState::Processing);
        assert_eq!(state(Some(2), Some(4)), RequestState::Partial);
        assert_eq!(state(Some(2), Some(5)), RequestState::Available);
        assert_eq!(state(Some(3), Some(2)), RequestState::Declined);
        assert_eq!(state(Some(4), Some(3)), RequestState::Failed);
        assert_eq!(state(Some(5), None), RequestState::Available);
    }
}
