//! Fake services for the collector tests, answering with the fixtures under
//! `tests/fixtures/<source>/`. Each fake checks the credential the real
//! service would check and answers everything else with what the real one
//! answers to a stranger, so a test that sends the wrong key fails the way
//! production would.
//!
//! Fixture dates are written as `@DAY+N@` / `@DAY-N@` and filled in relative
//! to now: the calendar window moves with the clock, the fixtures do not.

#![allow(dead_code)]

use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use queuewatch::collect::arr::{Arr, ArrKind};
use queuewatch::collect::{
    Source, http, prowlarr::Prowlarr, pyload::Pyload, qbittorrent::Qbittorrent, sabnzbd::Sabnzbd,
    seerr::Seerr, slskd::Slskd,
};
use wiremock::matchers::{any, body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const KEY: &str = "k3y-for-tests";
pub const USER: &str = "admin";
pub const PASSWORD: &str = "hunter2";

/// The markers the leak test looks for. They sit in fields the collector
/// must never copy: error messages, status messages, trackers, paths.
pub const MARKERS: &[&str] = &[
    "SEKRET123",
    "/data/",
    "tracker.example",
    "SecretTracker",
    "NZBSecret",
    "SecretHoster",
    "secret_sharer",
    "private-box",
    // An indexer's key in a URL that a client or a manager shows as the
    // NAME of a download (SABnzbd's "Trying <url>", a release title).
    "IDXKEY123",
    "idx.example",
    "getnzb",
    "announce",
];

pub fn fixture(name: &str) -> String {
    let file = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let mut text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
    let now = Timestamp::now();
    for n in -400i64..=400 {
        let token = if n < 0 {
            format!("@DAY{n}@")
        } else {
            format!("@DAY+{n}@")
        };
        if text.contains(&token) {
            let at = now + SignedDuration::from_hours(24 * n) + SignedDuration::from_mins(30);
            text = text.replace(&token, &at.to_string());
        }
    }
    text.replace("@DAY-0@", &now.to_string())
}

fn json(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_string(fixture(name))
}

/// What a service says to a request it did not expect or did not authorize.
async fn stranger(server: &MockServer, status: u16) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(status))
        .with_priority(10)
        .mount(server)
        .await;
}

pub fn client() -> reqwest::Client {
    http::client(Duration::from_secs(5)).unwrap()
}

pub async fn arr(kind: ArrKind) -> MockServer {
    let server = MockServer::start().await;
    let (dir, api, include, event) = match kind {
        ArrKind::Radarr => (
            "radarr",
            "/api/v3",
            "includeMovie",
            "downloadFolderImported",
        ),
        ArrKind::Sonarr => (
            "sonarr",
            "/api/v3",
            "includeSeries",
            "downloadFolderImported",
        ),
        ArrKind::Lidarr => ("lidarr", "/api/v1", "includeArtist", "downloadImported"),
    };
    Mock::given(method("GET"))
        .and(path(format!("{api}/queue")))
        .and(header("X-Api-Key", KEY))
        .and(query_param(include, "true"))
        .respond_with(json(&format!("{dir}/queue.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/calendar")))
        .and(header("X-Api-Key", KEY))
        .respond_with(json(&format!("{dir}/calendar.json")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/history/since")))
        .and(header("X-Api-Key", KEY))
        .and(query_param("eventType", event))
        .respond_with(json(&format!("{dir}/history.json")))
        .mount(&server)
        .await;
    stranger(&server, 401).await;
    server
}

pub fn arr_source(kind: ArrKind, label: &str, server: &MockServer) -> Source {
    Source::Arr(Arr::new(kind, label, &server.uri(), KEY.into(), client()).unwrap())
}

pub async fn prowlarr() -> MockServer {
    let server = MockServer::start().await;
    for (p, f) in [
        ("/api/v1/indexer", "prowlarr/indexer.json"),
        ("/api/v1/indexerstatus", "prowlarr/indexerstatus.json"),
    ] {
        Mock::given(method("GET"))
            .and(path(p))
            .and(header("X-Api-Key", KEY))
            .respond_with(json(f))
            .mount(&server)
            .await;
    }
    stranger(&server, 401).await;
    server
}

pub fn prowlarr_source(server: &MockServer) -> Source {
    Source::Prowlarr(Prowlarr::new("Indexers", &server.uri(), KEY.into(), client()).unwrap())
}

pub async fn sabnzbd() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("output", "json"))
        .and(query_param("apikey", KEY))
        .respond_with(json("sabnzbd/queue.json"))
        .mount(&server)
        .await;
    // SABnzbd answers a wrong key with 200 and a refusal.
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"status":false,"error":"API Key Incorrect"}"#),
        )
        .with_priority(10)
        .mount(&server)
        .await;
    server
}

pub fn sabnzbd_source(server: &MockServer) -> Source {
    Source::Sabnzbd(Sabnzbd::new("Usenet", &server.uri(), KEY.into(), client()).unwrap())
}

/// qBittorrent: the session cookie `QBT_SID_8080=<sid>` unlocks the list.
pub async fn qbittorrent(sid: &str) -> MockServer {
    let server = MockServer::start().await;
    mount_qbittorrent(&server, sid).await;
    server
}

pub async fn mount_qbittorrent(server: &MockServer, sid: &str) {
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .and(header("Referer", format!("{}/", server.uri()).as_str()))
        .and(body_string_contains(format!("username={USER}")))
        .and(body_string_contains(format!("password={PASSWORD}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "set-cookie",
                    format!("QBT_SID_8080={sid}; HttpOnly; path=/; SameSite=Lax").as_str(),
                )
                .set_body_string("Ok."),
        )
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(5)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .and(header("cookie", format!("QBT_SID_8080={sid}").as_str()))
        .respond_with(json("qbittorrent/torrents.json"))
        .mount(server)
        .await;
    stranger(server, 403).await;
}

pub fn qbittorrent_source(server: &MockServer, password: &str) -> Source {
    Source::Qbittorrent(
        Qbittorrent::new("Torrents", &server.uri(), USER, password.into(), client()).unwrap(),
    )
}

/// pyLoad: `GET /login` hands out a pre-session and the CSRF token, `POST
/// /login` with both turns it into a signed-in session.
pub async fn pyload() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("set-cookie", "session=pre; HttpOnly; Path=/")
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(fixture("pyload/login.html")),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .and(header("cookie", "session=pre"))
        .and(body_string_contains(
            "csrf_token=IjM0ZTVmNmE3YjhjOWQwIg.ZxYz.csrf-fixture",
        ))
        .and(body_string_contains(format!("username={USER}")))
        .and(body_string_contains(format!("password={PASSWORD}")))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "/dashboard")
                .insert_header("set-cookie", "session=authed; HttpOnly; Path=/"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/status_downloads"))
        .and(header("cookie", "session=authed"))
        .respond_with(json("pyload/status_downloads.json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/status_downloads"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error": "Invalid API credentials"}"#),
        )
        .with_priority(5)
        .mount(&server)
        .await;
    // A wrong password renders the form again.
    Mock::given(method("POST"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture("pyload/login.html")))
        .with_priority(5)
        .mount(&server)
        .await;
    server
}

pub fn pyload_source(server: &MockServer, password: &str) -> Source {
    Source::Pyload(Pyload::new("Hoster", &server.uri(), USER, password.into(), client()).unwrap())
}

pub async fn slskd() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v0/transfers/downloads"))
        .and(header("X-API-Key", KEY))
        .respond_with(json("slskd/downloads.json"))
        .mount(&server)
        .await;
    stranger(&server, 401).await;
    server
}

pub fn slskd_source(server: &MockServer) -> Source {
    Source::Slskd(Slskd::new("Soulseek", &server.uri(), KEY.into(), client()).unwrap())
}

pub async fn seerr() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/request"))
        .and(query_param("take", "100"))
        .and(header("X-Api-Key", KEY))
        .respond_with(json("seerr/request.json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/movie/693134"))
        .and(header("X-Api-Key", KEY))
        .respond_with(json("seerr/movie-693134.json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tv/95396"))
        .and(header("X-Api-Key", KEY))
        .respond_with(json("seerr/tv-95396.json"))
        .mount(&server)
        .await;
    // Unknown TMDB ids: Seerr answers 404.
    stranger(&server, 404).await;
    server
}

pub fn seerr_source(server: &MockServer) -> Source {
    Source::Seerr(Seerr::new("Requests", &server.uri(), KEY.into(), client()).unwrap())
}
