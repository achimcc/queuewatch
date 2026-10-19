//! qBittorrent: the torrents that are still downloading.
//!
//! WebUI API v2 as in qBittorrent 5.2.2 (`src/webui/api/authcontroller.cpp`,
//! `serialize_torrent.cpp`). Sign-in is a form POST that sets a session
//! cookie — named `QBT_SID_<port>` since 5.x, so the jar keeps whatever it
//! is given rather than looking for `SID`. Wrong credentials answer 401 (5.x)
//! or 200 `Fails.` (4.x); an expired session answers 403, and is renewed once.
//!
//! Not read: `tracker`, `save_path`, `content_path`, `magnet_uri`,
//! `comment` — every one of them can carry a passkey or a path.

use reqwest::Client;
use reqwest::header::{ORIGIN, REFERER};
use serde::Deserialize;
use url::Url;

use super::http::{self, CookieJar, FetchError};
use super::merge::{Contribution, Tracked};
use super::title::client_title;
use super::{Round, eta_in};
use crate::snapshot::{ItemState, MediaIds, MediaKind, Origin, QueueItem, Transport};

/// qBittorrent's "infinity" for the eta (`MAX_ETA`).
const NO_ETA: i64 = 8_640_000;

pub struct Qbittorrent {
    pub label: String,
    base: Url,
    username: String,
    password: String,
    http: Client,
    jar: CookieJar,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Torrent {
    hash: Option<String>,
    name: Option<String>,
    state: Option<String>,
    progress: Option<f64>,
    eta: Option<i64>,
    size: Option<i64>,
    amount_left: Option<i64>,
}

/// `None` for torrents that are done downloading: seeding is not a queue.
fn state(s: &str) -> Option<ItemState> {
    Some(match s {
        "uploading" | "stalledUP" | "pausedUP" | "stoppedUP" | "queuedUP" | "forcedUP"
        | "checkingUP" => {
            return None;
        }
        "downloading" | "forcedDL" | "metaDL" | "forcedMetaDL" => ItemState::Downloading,
        "stalledDL" => ItemState::Stalled,
        "pausedDL" | "stoppedDL" => ItemState::Paused,
        "error" | "missingFiles" => ItemState::Failed,
        "moving" => ItemState::Importing,
        _ => ItemState::Queued,
    })
}

impl Qbittorrent {
    pub fn new(
        label: &str,
        url: &str,
        username: &str,
        password: String,
        http: Client,
    ) -> anyhow::Result<Self> {
        Ok(Qbittorrent {
            label: label.to_string(),
            base: http::base_url(url)?,
            username: username.to_string(),
            password,
            http,
            jar: CookieJar::default(),
        })
    }

    async fn login(&self) -> Result<(), FetchError> {
        self.jar.clear();
        let url = http::endpoint(&self.base, "/api/v2/auth/login", &[]);
        // qBittorrent refuses a login whose Referer/Origin does not match
        // its own host (CSRF protection), so both name the base URL.
        let response = http::send(
            self.http
                .post(url)
                .header(REFERER, self.base.as_str())
                .header(ORIGIN, self.base.origin().ascii_serialization())
                .form(&[
                    ("username", self.username.as_str()),
                    ("password", self.password.as_str()),
                ]),
        )
        .await?;
        self.jar.store(response.headers());
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Status(status.as_u16()));
        }
        let body = http::text(response).await?;
        // No cookie check here: with "bypass authentication" for the
        // collector's subnet, qBittorrent answers Ok. without a session, and
        // the API works without one.
        if body.trim() == "Fails." {
            return Err(FetchError::Refused);
        }
        Ok(())
    }

    /// `Ok(None)` when the session is gone (403).
    async fn torrents(&self) -> Result<Option<Vec<Torrent>>, FetchError> {
        let url = http::endpoint(&self.base, "/api/v2/torrents/info", &[]);
        let response = http::send(self.jar.apply(self.http.get(url))).await?;
        self.jar.store(response.headers());
        if response.status().as_u16() == 403 {
            return Ok(None);
        }
        http::json(response).await.map(Some)
    }

    pub async fn fetch(&self, round: &Round) -> Result<Contribution, FetchError> {
        if self.jar.is_empty() {
            self.login().await?;
        }
        let torrents = match self.torrents().await? {
            Some(t) => t,
            None => {
                self.login().await?;
                self.torrents().await?.ok_or(FetchError::Status(403))?
            }
        };
        let client = torrents
            .iter()
            .filter_map(|t| {
                let state = state(t.state.as_deref().unwrap_or(""))?;
                // A magnet without metadata yet is named after whatever the
                // magnet carried, which can be its tracker URL.
                let title = client_title(t.name.as_deref())?;
                let eta = t
                    .eta
                    .filter(|e| *e > 0 && *e < NO_ETA)
                    .and_then(|e| eta_in(round.now, e as f64));
                Some(Tracked {
                    item: QueueItem {
                        kind: MediaKind::Other,
                        title,
                        ids: MediaIds::default(),
                        transport: Transport::Torrent,
                        state,
                        origin: Origin::Client,
                        progress: t.progress.unwrap_or(0.0).clamp(0.0, 1.0) as f32,
                        size_bytes: t.size.filter(|s| *s > 0).map(|s| s as u64),
                        left_bytes: t.amount_left.filter(|s| *s >= 0).map(|s| s as u64),
                        eta,
                    },
                    download_id: t.hash.clone(),
                })
            })
            .collect();
        Ok(Contribution {
            client,
            ..Default::default()
        })
    }
}
