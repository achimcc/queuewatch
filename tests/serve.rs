//! The page from the outside: routes, sessions, the snapshot and what each
//! person gets to see. No identity provider is running — the router is built
//! without one, and sessions are minted with the same function and key the
//! callback uses, so a test that gets in proves a browser would.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::{Key, PrivateCookieJar};
use queuewatch::serve::fetch::SnapshotSource;
use queuewatch::serve::i18n::{DE, EN, Lang};
use queuewatch::serve::oidc::PendingLogin;
use queuewatch::serve::session::{Session, pending_cookie, session_cookie};
use queuewatch::serve::{AppState, Settings, router};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{header as has_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "collector-token-for-tests";

fn key() -> Key {
    Key::from(&[42u8; 64])
}

struct Setup {
    lang: Lang,
    required_group: Option<&'static str>,
    home_url: Option<&'static str>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            lang: Lang::En,
            required_group: None,
            home_url: None,
        }
    }
}

fn app(snapshot_url: String, setup: Setup) -> axum::Router {
    let snapshots = SnapshotSource::new(snapshot_url, TOKEN, Duration::from_secs(15)).unwrap();
    router(AppState::new(
        key(),
        None,
        snapshots,
        Settings {
            public_url: "https://queue.example.org".into(),
            lang: setup.lang,
            required_group: setup.required_group.map(String::from),
            tz: jiff::tz::TimeZone::UTC,
            home_url: setup.home_url.map(String::from),
        },
    ))
}

/// The `Cookie` header a browser would send back after this jar was set.
fn cookie_header(jar: PrivateCookieJar) -> String {
    let response = jar.into_response();
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

fn session_for(username: &str, email: Option<&str>, groups: &[&str]) -> String {
    let session = Session {
        username: username.into(),
        email: email.map(String::from),
        groups: groups.iter().map(|g| g.to_string()).collect(),
        expires_at: jiff::Timestamp::now().as_second() + 3600,
    };
    cookie_header(PrivateCookieJar::new(key()).add(session_cookie(&session, true)))
}

async fn get(app: axum::Router, uri: &str, cookie: Option<&str>) -> Response {
    let mut req = Request::builder().uri(uri);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
}

async fn body_text(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The part of the page between two section anchors.
fn section<'a>(page: &'a str, id: &str, next: &str) -> &'a str {
    let start = page
        .find(&format!("id=\"{id}\""))
        .unwrap_or_else(|| panic!("no section {id}"));
    let end = page[start..]
        .find(&format!("id=\"{next}\""))
        .map(|e| start + e)
        .unwrap_or(page.len());
    &page[start..end]
}

fn snapshot(version: u32) -> serde_json::Value {
    let now = jiff::Timestamp::now();
    let later = now + jiff::SignedDuration::from_hours(2);
    json!({
        "version": version,
        "taken_at": now.to_string(),
        "sources": [
            { "kind": "radarr", "label": "Movies", "available": true },
            { "kind": "sabnzbd", "label": "Usenet downloads", "available": false }
        ],
        "queue": [
            {
                "kind": "movie", "title": "Ada's Movie In The Queue", "ids": {},
                "transport": "usenet", "state": "downloading", "origin": "manager",
                "progress": 0.42, "size_bytes": 4200000000u64, "eta": later.to_string()
            },
            {
                "kind": "episode", "title": "Konrad's Episode In The Queue", "ids": {},
                "transport": "torrent", "state": "stalled", "origin": "manager",
                "progress": 0.1
            }
        ],
        "indexers": { "enabled": 8, "healthy": 7, "known": true },
        "calendar": [
            { "kind": "episode", "title": "Upcoming Episode", "ids": {},
              "airs_at": later.to_string(), "has_file": false }
        ],
        "recent": [
            { "kind": "album", "title": "Fresh Album", "ids": {}, "imported_at": now.to_string() }
        ],
        "requests": [
            {
                "kind": "movie", "title": "Ada's Requested Movie", "ids": {},
                "requested_by": { "username": "Ada" },
                "requested_at": now.to_string(), "state": "processing",
                "progress": 0.42, "eta": later.to_string()
            },
            {
                "kind": "movie", "title": "Ada's Request By Address", "ids": {},
                "requested_by": { "username": "ada-at-seerr", "email": "ADA@example.org" },
                "requested_at": now.to_string(), "state": "pending"
            },
            {
                "kind": "episode", "title": "Konrad's Requested Series", "ids": {},
                "requested_by": { "username": "konrad", "email": "konrad@example.org" },
                "requested_at": now.to_string(), "state": "available"
            }
        ]
    })
}

async fn collector(body: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .and(has_header(
            "authorization",
            format!("Bearer {TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn without_a_session_the_page_sends_you_to_sign_in() {
    let response = get(
        app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
        "/",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/auth/login");
}

#[tokio::test]
async fn a_forged_cookie_is_no_session() {
    let session = Session {
        username: "ada".into(),
        email: None,
        groups: vec![],
        expires_at: i64::MAX,
    };
    let forged = cookie_header(
        PrivateCookieJar::new(Key::from(&[1u8; 64])).add(session_cookie(&session, true)),
    );
    let response = get(
        app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
        "/",
        Some(&forged),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn an_expired_session_is_no_session() {
    let session = Session {
        username: "ada".into(),
        email: None,
        groups: vec![],
        expires_at: jiff::Timestamp::now().as_second() - 1,
    };
    let old = cookie_header(PrivateCookieJar::new(key()).add(session_cookie(&session, true)));
    let response = get(
        app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
        "/",
        Some(&old),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_signed_in_person_sees_only_their_own_requests_but_the_whole_queue() {
    let server = collector(snapshot(queuewatch::snapshot::SCHEMA_VERSION)).await;
    let cookie = session_for("ada", Some("ada@example.org"), &[]);
    let response = get(
        app(format!("{}/snapshot", server.uri()), Setup::default()),
        "/",
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_text(response).await;

    let mine = section(&page, "mine", "queue");
    assert!(
        mine.contains("Ada&#39;s Requested Movie") || mine.contains("Ada's Requested Movie"),
        "{mine}"
    );
    assert!(
        mine.contains("Request By Address"),
        "matched by address, case-insensitively: {mine}"
    );
    assert!(
        !mine.contains("Konrad"),
        "another person's request leaked: {mine}"
    );

    let queue = section(&page, "queue", "calendar");
    assert!(queue.contains("Movie In The Queue"), "{queue}");
    assert!(
        queue.contains("Episode In The Queue"),
        "everyone's downloads: {queue}"
    );
    assert!(
        queue.contains("Usenet") && queue.contains("Torrent"),
        "{queue}"
    );
    assert!(queue.contains("4.2 GB"), "{queue}");
    assert!(queue.contains("<progress"), "{queue}");
    assert!(
        !queue.contains("konrad") && !queue.contains("ada-at-seerr"),
        "no requester in the queue"
    );

    assert!(section(&page, "calendar", "recent").contains("Upcoming Episode"));
    assert!(section(&page, "recent", "services").contains("Fresh Album"));
    let services = section(&page, "services", "</main>");
    assert!(services.contains("7/8 sources up"), "{services}");
    assert!(
        services.contains("Usenet downloads") && services.contains("not reachable"),
        "{services}"
    );

    // The header: who is signed in, how old the data is, a way out.
    assert!(page.contains("Signed in as"), "{page}");
    assert!(page.contains("action=\"/auth/logout\""));
    assert!(page.contains("updated"));
    assert!(page.contains("http-equiv=\"refresh\""));
    assert!(!page.contains("<script"), "no JavaScript");
    // And never the collector's token.
    assert!(!page.contains(TOKEN));
}

#[tokio::test]
async fn the_page_speaks_german_when_asked() {
    let server = collector(snapshot(queuewatch::snapshot::SCHEMA_VERSION)).await;
    let cookie = session_for("ada", None, &[]);
    let setup = Setup {
        lang: Lang::De,
        ..Setup::default()
    };
    let page = body_text(
        get(
            app(format!("{}/snapshot", server.uri()), setup),
            "/",
            Some(&cookie),
        )
        .await,
    )
    .await;
    for words in [
        DE.my_requests,
        DE.downloading,
        DE.coming_up,
        DE.recent,
        "Suche &amp; Dienste",
        "lang=\"de\"",
    ] {
        assert!(page.contains(words), "{words} missing");
    }
    assert!(!page.contains(EN.my_requests));
}

/// The collector's marker for a withheld name is English by construction;
/// the page shows it in its own language, and never the marker itself.
#[tokio::test]
async fn a_withheld_name_is_said_in_the_language_of_the_page() {
    let mut body = snapshot(queuewatch::snapshot::SCHEMA_VERSION);
    body["queue"][0]["title"] = json!(queuewatch::snapshot::UNNAMED);
    let server = collector(body).await;
    let cookie = session_for("ada", None, &[]);
    let setup = Setup {
        lang: Lang::De,
        ..Setup::default()
    };
    let page = body_text(
        get(
            app(format!("{}/snapshot", server.uri()), setup),
            "/",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let queue = section(&page, "queue", "calendar");
    assert!(queue.contains(DE.unnamed_download), "{queue}");
    assert!(!queue.contains(queuewatch::snapshot::UNNAMED), "{queue}");
}

#[tokio::test]
async fn without_the_required_group_the_answer_is_403() {
    let server = collector(snapshot(queuewatch::snapshot::SCHEMA_VERSION)).await;
    let setup = || Setup {
        required_group: Some("Friends"),
        ..Setup::default()
    };
    let outsider = session_for("ada", None, &["Others"]);
    let response = get(
        app(format!("{}/snapshot", server.uri()), setup()),
        "/",
        Some(&outsider),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let page = body_text(response).await;
    assert!(page.contains(EN.forbidden_title));
    assert!(
        !page.contains("Movie In The Queue"),
        "a refusal shows nothing of the snapshot"
    );

    let member = session_for("ada", None, &["Friends"]);
    let response = get(
        app(format!("{}/snapshot", server.uri()), setup()),
        "/",
        Some(&member),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_collector_that_is_down_still_gives_a_page() {
    // Nothing listens on port 9 (discard) in a test environment.
    let cookie = session_for("ada", None, &[]);
    let response = get(
        app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
        "/",
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_text(response).await;
    assert!(page.contains(EN.unreachable), "{page}");
    assert!(page.contains(EN.no_data_yet));
}

#[tokio::test]
async fn a_snapshot_of_another_version_counts_as_unreachable() {
    let server = collector(snapshot(queuewatch::snapshot::SCHEMA_VERSION + 1)).await;
    let cookie = session_for("ada", None, &[]);
    let response = get(
        app(format!("{}/snapshot", server.uri()), Setup::default()),
        "/",
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_text(response).await;
    assert!(page.contains(EN.unreachable), "{page}");
    assert!(
        !page.contains("Movie In The Queue"),
        "a snapshot it does not understand is not shown"
    );
}

#[tokio::test]
async fn a_collector_that_refuses_the_token_counts_as_unreachable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let cookie = session_for("ada", None, &[]);
    let page = body_text(
        get(
            app(format!("{}/snapshot", server.uri()), Setup::default()),
            "/",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(page.contains(EN.unreachable));
}

#[tokio::test]
async fn the_last_good_snapshot_is_shown_when_the_collector_goes_away() {
    // One good answer, then only errors.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(snapshot(queuewatch::snapshot::SCHEMA_VERSION)),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let snapshots = SnapshotSource::new(
        format!("{}/snapshot", server.uri()),
        TOKEN,
        Duration::from_millis(1),
    )
    .unwrap();
    let first = snapshots.get().await;
    assert!(first.reachable && first.snapshot.is_some());
    tokio::time::sleep(Duration::from_millis(5)).await;
    let second = snapshots.get().await;
    assert!(!second.reachable);
    assert_eq!(second.snapshot, first.snapshot, "the last good one stays");
}

#[tokio::test]
async fn every_response_carries_the_security_headers() {
    let cookie = session_for("ada", None, &[]);
    for (uri, cookie) in [
        ("/", Some(cookie.as_str())),
        ("/", None),
        ("/healthz", None),
        ("/style.css", None),
        ("/nope", None),
    ] {
        let response = get(
            app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
            uri,
            cookie,
        )
        .await;
        let h = response.headers();
        assert_eq!(
            h[header::CONTENT_SECURITY_POLICY],
            "default-src 'none'; style-src 'self'; img-src 'self' data:; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'",
            "{uri}"
        );
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{uri}");
        assert_eq!(h[header::REFERRER_POLICY], "no-referrer", "{uri}");
        if uri == "/style.css" {
            assert!(
                h[header::CACHE_CONTROL]
                    .to_str()
                    .unwrap()
                    .contains("max-age=31536000")
            );
        } else {
            assert_eq!(h[header::CACHE_CONTROL], "no-store", "{uri}");
        }
    }
}

#[tokio::test]
async fn healthz_needs_no_session() {
    let response = get(
        app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
        "/healthz",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "ok");
}

#[tokio::test]
async fn a_callback_with_a_mismatched_state_is_refused() {
    let pending = PendingLogin {
        state: "the-state-we-sent".into(),
        nonce: "n".into(),
        pkce_verifier: "v".into(),
    };
    let cookie = cookie_header(PrivateCookieJar::new(key()).add(pending_cookie(&pending, true)));
    let response = get(
        app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
        "/auth/callback?code=c&state=the-state-we-got",
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response.headers().get(header::SET_COOKIE).is_none(),
        "no session comes out of a refused callback"
    );
}

#[tokio::test]
async fn a_callback_nobody_started_is_refused() {
    let response = get(
        app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
        "/auth/callback?code=c&state=s",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.headers().get(header::SET_COOKIE).is_none());
}

#[tokio::test]
async fn a_provider_refusal_is_said_and_not_repeated() {
    let response = get(
        app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
        "/auth/callback?error=%3Cscript%3Ealert(1)%3C/script%3E",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let page = body_text(response).await;
    assert!(!page.contains("alert(1)"), "{page}");
}

async fn post_logout(cookie: &str, extra: &[(&str, &str)]) -> Response {
    let mut req = Request::builder()
        .method("POST")
        .uri("/auth/logout")
        .header(header::COOKIE, cookie);
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    app("http://127.0.0.1:9/snapshot".into(), Setup::default())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn signing_out_clears_the_cookie() {
    let cookie = session_for("ada", None, &[]);
    let response = post_logout(&cookie, &[("sec-fetch-site", "same-origin")]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let set = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(set.starts_with("queuewatch_session=;"), "{set}");
    assert!(
        set.contains("Max-Age=0") || set.contains("expires="),
        "{set}"
    );
}

#[tokio::test]
async fn a_cross_site_sign_out_is_refused() {
    let cookie = session_for("ada", None, &[]);
    let response = post_logout(&cookie, &[("sec-fetch-site", "cross-site")]).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get(header::SET_COOKIE).is_none());

    let response = post_logout(&cookie, &[("origin", "https://evil.example")]).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn signing_in_without_a_provider_says_so() {
    let response = get(
        app("http://127.0.0.1:9/snapshot".into(), Setup::default()),
        "/auth/login",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// The Content-Security-Policy forbids inline styles, so the page must not
/// rely on one anywhere — a `style=` would simply be dropped by the browser.
#[tokio::test]
async fn the_page_carries_no_inline_style() {
    let server = collector(snapshot(queuewatch::snapshot::SCHEMA_VERSION)).await;
    let cookie = session_for("ada", None, &[]);
    for lang in [Lang::En, Lang::De] {
        let setup = Setup {
            lang,
            ..Setup::default()
        };
        let page = body_text(
            get(
                app(format!("{}/snapshot", server.uri()), setup),
                "/",
                Some(&cookie),
            )
            .await,
        )
        .await;
        assert!(!page.contains("style="), "{page}");
        assert!(!page.contains("<style"), "{page}");
    }
}

/// The way back to the site's start page: there when configured, in the
/// page's terminal voice, with a spoken label; absent otherwise.
#[tokio::test]
async fn the_header_links_home_only_when_configured() {
    let server = collector(snapshot(queuewatch::snapshot::SCHEMA_VERSION)).await;
    let cookie = session_for("ada", None, &[]);
    let with = Setup {
        lang: Lang::De,
        home_url: Some("https://home.example.org"),
        ..Setup::default()
    };
    let page = body_text(
        get(
            app(format!("{}/snapshot", server.uri()), with),
            "/",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(
        page.contains(r#"href="https://home.example.org""#),
        "{page}"
    );
    assert!(page.contains("cd ~/home.example.org"), "{page}");
    assert!(
        page.contains(r#"aria-label="Zurück zu home.example.org""#),
        "{page}"
    );

    let page = body_text(
        get(
            app(format!("{}/snapshot", server.uri()), Setup::default()),
            "/",
            Some(&cookie),
        )
        .await,
    )
    .await;
    assert!(!page.contains(r#"class="home""#), "{page}");
}

/// The ASCII readout beside each `<progress>`: decoration, hidden from
/// screen readers, and the `<progress>` itself carries a label.
#[tokio::test]
async fn the_ascii_bar_renders_empty_half_and_full() {
    let mut body = snapshot(queuewatch::snapshot::SCHEMA_VERSION);
    body["queue"] = json!([
        { "kind": "movie", "title": "Empty", "ids": {}, "transport": "usenet",
          "state": "queued", "origin": "manager", "progress": 0.0 },
        { "kind": "movie", "title": "Half", "ids": {}, "transport": "usenet",
          "state": "downloading", "origin": "manager", "progress": 0.52 },
        { "kind": "movie", "title": "Full", "ids": {}, "transport": "usenet",
          "state": "importing", "origin": "manager", "progress": 1.0 }
    ]);
    let server = collector(body).await;
    let cookie = session_for("ada", None, &[]);
    let page = body_text(
        get(
            app(format!("{}/snapshot", server.uri()), Setup::default()),
            "/",
            Some(&cookie),
        )
        .await,
    )
    .await;
    let queue = section(&page, "queue", "calendar");
    for (bar, pct) in [
        ("[----------------]", "0%"),
        ("[########--------]", "52%"),
        ("[################]", "100%"),
    ] {
        assert!(
            queue.contains(&format!(
                "<span class=\"ascii\" aria-hidden=\"true\">{bar}</span><span class=\"pct\">{pct}</span>"
            )),
            "{bar} {pct} missing: {queue}"
        );
    }
    assert!(queue.contains("aria-label=\"Progress: Half\""), "{queue}");
}

/// Not a test: writes a realistic page, in English and German, plus the
/// stylesheet, for looking at in a browser.
///
/// `QUEUEWATCH_DUMP_DIR=<dir> cargo test --test serve -- --ignored dump_page`
/// writes `<dir>/en/index.html`, `<dir>/de/index.html`,
/// `<dir>/en-down/index.html` (collector gone, last good data shown) and
/// `<dir>/style.css`; serve `<dir>` and open `/en/`.
#[tokio::test]
#[ignore]
async fn dump_page() {
    let Ok(dir) = std::env::var("QUEUEWATCH_DUMP_DIR") else {
        eprintln!("QUEUEWATCH_DUMP_DIR is not set; nothing written");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let now = jiff::Timestamp::now();
    let mins = |m: i64| now + jiff::SignedDuration::from_mins(m);
    let long_name = "The.Remarkably.Long.Release.Name.Without.A.Single.Space.2026.\
                     German.DL.2160p.UHD.BluRay.HDR10Plus.DV.x265-SOMEGROUP.mkv";
    let long_name = &format!("{long_name}{}", "X".repeat(120 - long_name.len()));
    assert_eq!(long_name.len(), 120);
    let body = json!({
        "version": queuewatch::snapshot::SCHEMA_VERSION,
        "taken_at": mins(-3).to_string(),
        "sources": [
            { "kind": "radarr", "label": "Movies", "available": true },
            { "kind": "sonarr", "label": "Series", "available": true },
            { "kind": "lidarr", "label": "Music", "available": true },
            { "kind": "prowlarr", "label": "Search", "available": true },
            { "kind": "sabnzbd", "label": "Usenet downloads", "available": true },
            { "kind": "qbittorrent", "label": "Torrent downloads", "available": false },
            { "kind": "seerr", "label": "Requests", "available": true }
        ],
        "queue": [
            { "kind": "movie", "title": "Arrival", "ids": {}, "transport": "usenet",
              "state": "downloading", "origin": "manager", "progress": 0.52,
              "size_bytes": 14_300_000_000u64, "eta": mins(38).to_string() },
            { "kind": "other", "title": long_name, "ids": {}, "transport": "torrent",
              "state": "stalled", "origin": "client", "progress": 0.07,
              "size_bytes": 61_000_000_000u64 },
            { "kind": "episode", "title": "Severance S02E04", "ids": {}, "transport": "usenet",
              "state": "queued", "origin": "manager", "progress": 0.0 },
            { "kind": "album", "title": "Radiohead — In Rainbows", "ids": {},
              "transport": "soulseek", "state": "importing", "origin": "manager",
              "progress": 1.0, "size_bytes": 412_000_000u64 },
            { "kind": "movie", "title": "Paused Movie", "ids": {}, "transport": "hoster",
              "state": "paused", "origin": "manager", "progress": 0.31 },
            { "kind": "episode", "title": "Andor S02E01", "ids": {}, "transport": "torrent",
              "state": "failed", "origin": "manager", "progress": 0.88 }
        ],
        "indexers": { "enabled": 8, "healthy": 7, "known": true },
        "calendar": [
            { "kind": "episode", "title": "Slow Horses S05E03", "ids": {},
              "airs_at": mins(300).to_string(), "has_file": false },
            { "kind": "movie", "title": "Dune: Part Three", "ids": {},
              "airs_at": mins(60 * 24 * 4).to_string(), "has_file": false },
            { "kind": "episode", "title": "The Bear S04E01", "ids": {},
              "airs_at": mins(60 * 26).to_string(), "has_file": true }
        ],
        "recent": [
            { "kind": "movie", "title": "Perfect Days", "ids": {}, "imported_at": mins(-12).to_string() },
            { "kind": "album", "title": "Bon Iver — SABLE, fABLE", "ids": {}, "imported_at": mins(-190).to_string() },
            { "kind": "episode", "title": "Shogun S01E10", "ids": {}, "imported_at": mins(-60 * 30).to_string() }
        ],
        "requests": [
            { "kind": "movie", "title": "Arrival", "ids": {},
              "requested_by": { "username": "ada" }, "requested_at": mins(-60 * 5).to_string(),
              "state": "processing", "progress": 0.52, "eta": mins(38).to_string() },
            { "kind": "movie", "title": "Past Lives", "ids": {},
              "requested_by": { "username": "ada" }, "requested_at": mins(-60 * 30).to_string(),
              "state": "pending" },
            { "kind": "episode", "title": "Severance", "ids": {},
              "requested_by": { "username": "ada" }, "requested_at": mins(-60 * 80).to_string(),
              "state": "partial" },
            { "kind": "movie", "title": "Perfect Days", "ids": {},
              "requested_by": { "username": "ada" }, "requested_at": mins(-60 * 100).to_string(),
              "state": "available" },
            { "kind": "movie", "title": "Some Obscure Film", "ids": {},
              "requested_by": { "username": "ada" }, "requested_at": mins(-60 * 200).to_string(),
              "state": "declined" },
            { "kind": "album", "title": "Lost Album", "ids": {},
              "requested_by": { "username": "ada" }, "requested_at": mins(-60 * 250).to_string(),
              "state": "failed" }
        ]
    });
    let cookie = session_for("ada", None, &[]);
    let server = collector(body.clone()).await;
    for (name, lang) in [("en", Lang::En), ("de", Lang::De)] {
        let setup = Setup {
            lang,
            ..Setup::default()
        };
        let page = body_text(
            get(
                app(format!("{}/snapshot", server.uri()), setup),
                "/",
                Some(&cookie),
            )
            .await,
        )
        .await;
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("index.html"), page).unwrap();
    }

    // The collector answers once, then goes away: the page keeps the last
    // good snapshot and says so.
    let gone = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&gone)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&gone)
        .await;
    let snapshots = SnapshotSource::new(
        format!("{}/snapshot", gone.uri()),
        TOKEN,
        Duration::from_millis(1),
    )
    .unwrap();
    let down = router(AppState::new(
        key(),
        None,
        snapshots,
        Settings {
            public_url: "https://queue.example.org".into(),
            lang: Lang::En,
            required_group: None,
            tz: jiff::tz::TimeZone::UTC,
            home_url: None,
        },
    ));
    let _ = get(down.clone(), "/", Some(&cookie)).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let page = body_text(get(down.clone(), "/", Some(&cookie)).await).await;
    assert!(page.contains(EN.unreachable), "{page}");
    std::fs::create_dir_all(dir.join("en-down")).unwrap();
    std::fs::write(dir.join("en-down").join("index.html"), page).unwrap();

    let css = body_text(get(down, "/style.css", None).await).await;
    std::fs::write(dir.join("style.css"), css).unwrap();
}
