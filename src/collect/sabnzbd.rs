//! SABnzbd: the Usenet queue.
//!
//! `api?mode=queue&output=json`, shape from SABnzbd 5.1.2's
//! `sabnzbd/api.py::build_queue`. The API key travels in the query string —
//! SABnzbd knows no header for it — which is why no URL ever reaches an
//! error message (see [`super::http`]). A slot also carries the job's
//! `password` and `script`; neither is read.

use reqwest::Client;
use serde::Deserialize;
use url::Url;

use super::http::{self, FetchError};
use super::merge::{Contribution, Tracked};
use super::title::client_title;
use super::{Round, eta_in, fraction};
use crate::snapshot::{ItemState, MediaIds, MediaKind, Origin, QueueItem, Transport};

const MIB: f64 = 1024.0 * 1024.0;

pub struct Sabnzbd {
    pub label: String,
    base: Url,
    key: String,
    http: Client,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Answer {
    /// Absent when SABnzbd refuses (`{"status": false, "error": …}`).
    queue: Option<Queue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Queue {
    paused: Option<bool>,
    slots: Vec<Slot>,
}

/// SABnzbd writes its numbers as strings ("1234.56").
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Slot {
    nzo_id: Option<String>,
    filename: Option<String>,
    status: Option<String>,
    mb: Option<String>,
    mbleft: Option<String>,
    timeleft: Option<String>,
}

fn number(s: &Option<String>) -> Option<f64> {
    s.as_deref()?.trim().parse().ok()
}

/// `[D:]H:MM:SS` → seconds.
fn seconds(timeleft: &str) -> Option<f64> {
    let parts: Vec<u64> = timeleft
        .trim()
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let total = match parts.as_slice() {
        [d, h, m, s] => ((d * 24 + h) * 60 + m) * 60 + s,
        [h, m, s] => (h * 60 + m) * 60 + s,
        [m, s] => m * 60 + s,
        _ => return None,
    };
    Some(total as f64)
}

/// The statuses of `sabnzbd/constants.py::Status`. The post-processing ones
/// show up in the history, not the queue, but are mapped anyway.
fn state(status: &str) -> ItemState {
    match status {
        "Downloading" | "Fetching" => ItemState::Downloading,
        "Paused" => ItemState::Paused,
        "Failed" => ItemState::Failed,
        "Completed" => ItemState::Completed,
        "Extracting" | "Moving" | "QuickCheck" | "Repairing" | "Running" | "Verifying" => {
            ItemState::Importing
        }
        _ => ItemState::Queued,
    }
}

impl Sabnzbd {
    pub fn new(label: &str, url: &str, key: String, http: Client) -> anyhow::Result<Self> {
        Ok(Sabnzbd {
            label: label.to_string(),
            base: http::base_url(url)?,
            key,
            http,
        })
    }

    pub async fn fetch(&self, round: &Round) -> Result<Contribution, FetchError> {
        let url = http::endpoint(
            &self.base,
            "/api",
            &[("mode", "queue"), ("output", "json"), ("apikey", &self.key)],
        );
        let answer: Answer = http::get_json(self.http.get(url)).await?;
        let queue = answer.queue.ok_or(FetchError::Refused)?;
        let all_paused = queue.paused.unwrap_or(false);
        let client = queue
            .slots
            .iter()
            .filter_map(|slot| {
                // A job added by URL is named "Trying <url>" until SABnzbd
                // has fetched it — with the indexer's key in the URL.
                let title = client_title(slot.filename.as_deref())?;
                let mb = number(&slot.mb).unwrap_or(0.0);
                let mbleft = number(&slot.mbleft).unwrap_or(0.0);
                let mut state = state(slot.status.as_deref().unwrap_or(""));
                // With the whole queue paused, a slot keeps its own status
                // ("Queued"), which would read as "waiting for its turn".
                if all_paused && matches!(state, ItemState::Queued | ItemState::Downloading) {
                    state = ItemState::Paused;
                }
                let eta = if state == ItemState::Downloading {
                    slot.timeleft
                        .as_deref()
                        .and_then(seconds)
                        .and_then(|s| eta_in(round.now, s))
                } else {
                    None
                };
                Some(Tracked {
                    item: QueueItem {
                        kind: MediaKind::Other,
                        title,
                        ids: MediaIds::default(),
                        transport: Transport::Usenet,
                        state,
                        origin: Origin::Client,
                        progress: fraction(mb - mbleft, mb),
                        size_bytes: Some((mb * MIB) as u64).filter(|b| *b > 0),
                        left_bytes: Some((mbleft * MIB) as u64),
                        eta,
                    },
                    download_id: slot.nzo_id.clone(),
                })
            })
            .collect();
        Ok(Contribution {
            client,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_left_parses_with_and_without_days() {
        assert_eq!(seconds("0:10:22"), Some(622.0));
        assert_eq!(seconds("1:02:00:00"), Some(93600.0));
        assert_eq!(seconds("x"), None);
    }
}
