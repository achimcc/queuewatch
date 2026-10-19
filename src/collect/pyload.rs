//! pyLoad: the files a hoster is sending right now.
//!
//! pyload-ng 0.5.0b3.dev101 (`webui/app/blueprints/api_blueprint.py`,
//! `helpers.py::apikey_auth`). Its `/api/login` answers "Obsolete API"; the
//! API takes either an API key (`X-API-Key: pl_…`) or the web session. With
//! a user name and password the way in is the web form: `GET /login` for the
//! CSRF token Flask-WTF wants, `POST /login`, then the session cookie. pyLoad
//! renews that cookie on every answer, so the jar is updated on every answer.
//!
//! Not read: `statusmsg`, `plugin`, `format_*`, `info` — text for a person
//! at pyLoad's own page, and the plugin name is the hoster's name.

use reqwest::Client;
use reqwest::header::REFERER;
use serde::Deserialize;
use url::Url;

use super::http::{self, CookieJar, FetchError};
use super::merge::{Contribution, Tracked};
use super::title::client_title;
use super::{Round, eta_in, fraction};
use crate::snapshot::{ItemState, MediaIds, MediaKind, Origin, QueueItem, Transport};

pub struct Pyload {
    pub label: String,
    base: Url,
    username: String,
    password: String,
    http: Client,
    jar: CookieJar,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DownloadInfo {
    name: Option<String>,
    eta: Option<i64>,
    bleft: Option<i64>,
    size: Option<i64>,
    status: Option<i64>,
}

/// `core/datatypes/enums.py::DownloadStatus`.
fn state(status: i64) -> ItemState {
    match status {
        12 => ItemState::Downloading,
        0 | 4 => ItemState::Completed,
        1 | 8 | 9 => ItemState::Failed,
        6 => ItemState::Stalled,
        13 => ItemState::Importing,
        // 2 online, 3 queued, 5 waiting (the hoster's cooldown), 7 starting,
        // 10 decrypting, 11 custom, 14 unknown.
        _ => ItemState::Queued,
    }
}

/// The CSRF token from the login page: the hidden form field, or the
/// `<meta name="csrf-token">` every page carries.
fn csrf_token(html: &str) -> Option<String> {
    for (marker, attribute) in [
        ("name=\"csrf_token\"", "value=\""),
        ("name=\"csrf-token\"", "content=\""),
    ] {
        let Some(at) = html.find(marker) else {
            continue;
        };
        // The attribute may stand before or after the name within the tag.
        let tag_start = html[..at].rfind('<').unwrap_or(0);
        let tag_end = html[at..].find('>').map_or(html.len(), |e| at + e);
        let tag = &html[tag_start..tag_end];
        if let Some(v) = tag.find(attribute) {
            let rest = &tag[v + attribute.len()..];
            if let Some(end) = rest.find('"') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

impl Pyload {
    pub fn new(
        label: &str,
        url: &str,
        username: &str,
        password: String,
        http: Client,
    ) -> anyhow::Result<Self> {
        Ok(Pyload {
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
        let url = http::endpoint(&self.base, "/login", &[]);
        let page = http::send(self.http.get(url.clone())).await?;
        self.jar.store(page.headers());
        // A redirect here means pyLoad signed us in by itself (`autologin`).
        if page.status().is_redirection() {
            return Ok(());
        }
        if !page.status().is_success() {
            return Err(FetchError::Status(page.status().as_u16()));
        }
        let html = http::text(page).await?;
        let token = csrf_token(&html).ok_or(FetchError::Refused)?;
        let answer = http::send(
            self.jar
                .apply(self.http.post(url))
                .header(REFERER, self.base.as_str())
                .form(&[
                    ("username", self.username.as_str()),
                    ("password", self.password.as_str()),
                    ("csrf_token", token.as_str()),
                ]),
        )
        .await?;
        self.jar.store(answer.headers());
        // Success redirects to the dashboard; a wrong password renders the
        // form again with 200. Which one it was, the API call says next.
        let status = answer.status();
        if status.is_success() || status.is_redirection() {
            Ok(())
        } else {
            Err(FetchError::Status(status.as_u16()))
        }
    }

    /// `Ok(None)` when pyLoad does not know the session (401).
    async fn downloads(&self) -> Result<Option<Vec<DownloadInfo>>, FetchError> {
        let url = http::endpoint(&self.base, "/api/status_downloads", &[]);
        let response = http::send(self.jar.apply(self.http.get(url))).await?;
        self.jar.store(response.headers());
        if response.status().as_u16() == 401 {
            return Ok(None);
        }
        http::json(response).await.map(Some)
    }

    pub async fn fetch(&self, round: &Round) -> Result<Contribution, FetchError> {
        let downloads = match self.downloads().await? {
            Some(d) => d,
            None => {
                self.login().await?;
                self.downloads().await?.ok_or(FetchError::Status(401))?
            }
        };
        let client = downloads
            .iter()
            .filter_map(|d| {
                // Until pyLoad has checked a link, its name is the link.
                let title = client_title(d.name.as_deref())?;
                let size = d.size.unwrap_or(0).max(0);
                let left = d.bleft.unwrap_or(0).max(0);
                let state = state(d.status.unwrap_or(14));
                let eta = match state {
                    ItemState::Downloading => d.eta.and_then(|e| eta_in(round.now, e as f64)),
                    _ => None,
                };
                Some(Tracked {
                    item: QueueItem {
                        kind: MediaKind::Other,
                        title,
                        ids: MediaIds::default(),
                        transport: Transport::Hoster,
                        state,
                        origin: Origin::Client,
                        progress: fraction((size - left) as f64, size as f64),
                        size_bytes: Some(size as u64).filter(|s| *s > 0),
                        left_bytes: Some(left as u64),
                        eta,
                    },
                    // No manager hands work to pyLoad, so nothing to join on.
                    download_id: None,
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
    fn the_csrf_token_is_found_in_the_form_or_the_meta_tag() {
        let form = r#"<form><input type="hidden" name="csrf_token" value="abc.123"/></form>"#;
        assert_eq!(csrf_token(form).as_deref(), Some("abc.123"));
        let meta = r#"<head><meta content="xyz" name="csrf-token"></head>"#;
        assert_eq!(csrf_token(meta).as_deref(), Some("xyz"));
        assert_eq!(csrf_token("<html></html>"), None);
    }
}
