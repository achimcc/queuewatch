//! The sign-in against a fake provider: the redirect out, the cookie it
//! leaves, a callback that does not end in a session when the provider will
//! not exchange the code — and, with tokens signed by the client secret
//! (HS256, so that no private key has to live in the repository), one that
//! does, and one whose signature points at a key nobody knows yet.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum_extra::extract::cookie::Key;
use queuewatch::serve::fetch::SnapshotSource;
use queuewatch::serve::i18n::Lang;
use queuewatch::serve::oidc::{OidcSettings, Provider};
use queuewatch::serve::{AppState, Settings, router};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn provider(server: &MockServer) -> Provider {
    let issuer = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "keys": [] })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": "invalid_grant" })))
        .mount(server)
        .await;
    Provider::discover(&OidcSettings {
        issuer,
        client_id: "queuewatch".into(),
        client_secret: "s3cret".into(),
        redirect_uri: "http://queue.test/auth/callback".into(),
        user_claim: "preferred_username".into(),
        group_claim: "groups".into(),
    })
    .await
    .expect("discovery")
}

fn app(provider: Provider, public_url: &str) -> axum::Router {
    app_requiring(provider, public_url, None)
}

fn app_requiring(provider: Provider, public_url: &str, group: Option<&str>) -> axum::Router {
    router(AppState::new(
        Key::from(&[42u8; 64]),
        Some(provider),
        SnapshotSource::new(
            "http://127.0.0.1:9/snapshot".into(),
            "t",
            Duration::from_secs(15),
        )
        .unwrap(),
        Settings {
            public_url: public_url.into(),
            lang: Lang::En,
            required_group: group.map(String::from),
            tz: jiff::tz::TimeZone::UTC,
            home_url: None,
        },
    ))
}

async fn get(app: axum::Router, uri: &str, cookie: Option<&str>) -> axum::response::Response {
    let mut req = Request::builder().uri(uri);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
}

#[tokio::test]
async fn the_login_redirects_to_the_provider_and_leaves_a_scoped_private_cookie() {
    let server = MockServer::start().await;
    let response = get(
        app(provider(&server).await, "https://queue.example.org"),
        "/auth/login",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()[header::LOCATION].to_str().unwrap();
    assert!(
        location.starts_with(&format!("{}/authorize", server.uri())),
        "{location}"
    );

    let set = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(set.starts_with("queuewatch_pending="), "{set}");
    for attr in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/auth",
        "Max-Age=600",
    ] {
        assert!(set.contains(attr), "{attr} missing: {set}");
    }
    // Encrypted: the state in the URL is not readable in the cookie.
    let state = location
        .split(['?', '&'])
        .find_map(|p| p.strip_prefix("state="))
        .expect("state in the URL");
    assert!(!set.contains(state), "{set}");
}

#[tokio::test]
async fn over_plain_http_the_cookie_is_not_secure() {
    let server = MockServer::start().await;
    let response = get(
        app(provider(&server).await, "http://queue.test"),
        "/auth/login",
        None,
    )
    .await;
    let set = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        !set.contains("Secure"),
        "a Secure cookie would never come back over http: {set}"
    );
}

#[tokio::test]
async fn a_callback_the_provider_will_not_exchange_ends_without_a_session() {
    let server = MockServer::start().await;
    let app = app(provider(&server).await, "https://queue.example.org");
    let login = get(app.clone(), "/auth/login", None).await;
    let location = login.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string();
    let state = location
        .split(['?', '&'])
        .find_map(|p| p.strip_prefix("state="))
        .unwrap()
        .to_string();
    let cookie = login.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    let response = get(
        app,
        &format!("/auth/callback?code=c&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get(header::SET_COOKIE).is_none());
}

const SECRET: &str = "s3cret";

/// Discovery for a provider that signs its ID tokens with `alg`.
async fn mount_discovery(server: &MockServer, alg: &str) {
    let issuer = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": [alg],
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "keys": [] })))
        .mount(server)
        .await;
}

async fn discovered(server: &MockServer) -> Provider {
    Provider::discover(&OidcSettings {
        issuer: server.uri(),
        client_id: "queuewatch".into(),
        client_secret: SECRET.into(),
        redirect_uri: "https://queue.example.org/auth/callback".into(),
        user_claim: "preferred_username".into(),
        group_claim: "groups".into(),
    })
    .await
    .expect("discovery")
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A JWT with this header and these claims. HS256 is signed with the client
/// secret, as the provider would; anything else gets a signature of junk.
fn jwt(header: serde_json::Value, claims: serde_json::Value) -> String {
    use openidconnect::PrivateSigningKey as _;
    use openidconnect::core::{CoreHmacKey, CoreJwsSigningAlgorithm};
    let input = format!(
        "{}.{}",
        b64(&serde_json::to_vec(&header).unwrap()),
        b64(&serde_json::to_vec(&claims).unwrap())
    );
    let signature = if header["alg"] == "HS256" {
        CoreHmacKey::new(SECRET)
            .sign(&CoreJwsSigningAlgorithm::HmacSha256, input.as_bytes())
            .unwrap()
    } else {
        b"not a signature".to_vec()
    };
    format!("{input}.{}", b64(&signature))
}

/// `/auth/login`: the `Cookie` header to send back, the state and the nonce.
async fn start_login(app: &axum::Router) -> (String, String, String) {
    let login = get(app.clone(), "/auth/login", None).await;
    let location = login.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string();
    let param = |name: &str| {
        location
            .split(['?', '&'])
            .find_map(|p| p.strip_prefix(&format!("{name}=")))
            .unwrap()
            .to_string()
    };
    let cookie = login.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    (cookie, param("state"), param("nonce"))
}

fn claims(server: &MockServer, nonce: &str, groups: Vec<String>) -> serde_json::Value {
    let now = jiff::Timestamp::now().as_second();
    json!({
        "iss": server.uri(),
        "aud": "queuewatch",
        "sub": "df55fb1b",
        "iat": now,
        "exp": now + 300,
        "nonce": nonce,
        "preferred_username": "ada",
        "email": "ada@example.org",
        "email_verified": true,
        "groups": groups,
    })
}

async fn token_answers(server: &MockServer, id_token: String) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "at",
            "token_type": "Bearer",
            "expires_in": 300,
            "id_token": id_token,
        })))
        .mount(server)
        .await;
}

/// THE WHOLE WAY IN, for someone in three hundred groups: the session cookie
/// holds the one group the page asks about, and so stays far below the
/// browser's 4 KB — with every group in it, it would not.
#[tokio::test]
async fn a_sign_in_keeps_only_the_required_group_in_the_cookie() {
    use axum::http::HeaderMap;
    use axum_extra::extract::cookie::PrivateCookieJar;
    use queuewatch::serve::session::read_session;

    let server = MockServer::start().await;
    mount_discovery(&server, "HS256").await;
    let app = app_requiring(
        discovered(&server).await,
        "https://queue.example.org",
        Some("friends"),
    );
    let (cookie, state, nonce) = start_login(&app).await;
    let groups: Vec<String> = (0..300)
        .map(|i| format!("department-{i:03}"))
        .chain(["friends".to_string()])
        .collect();
    token_answers(
        &server,
        jwt(
            json!({ "alg": "HS256", "typ": "JWT" }),
            claims(&server, &nonce, groups),
        ),
    )
    .await;

    let response = get(
        app,
        &format!("/auth/callback?code=c&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let session = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with("queuewatch_session="))
        .expect("a session cookie")
        .to_string();
    assert!(session.len() < 1024, "{} bytes", session.len());

    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        session.split(';').next().unwrap().parse().unwrap(),
    );
    let jar = PrivateCookieJar::from_headers(&headers, Key::from(&[42u8; 64]));
    let who = read_session(&jar, jiff::Timestamp::now().as_second()).expect("a session");
    assert_eq!(who.username, "ada");
    assert_eq!(who.groups, ["friends"]);
    assert_eq!(who.email.as_deref(), Some("ada@example.org"));
}

fn discoveries(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.url.path() == "/.well-known/openid-configuration")
        .count()
}

/// A TOKEN SIGNED WITH A KEY THE PAGE DOES NOT KNOW is what a key rotation
/// looks like from here: the sign-in fails, and the page asks the provider
/// again — once, however many such callbacks follow within the minute.
#[tokio::test]
async fn a_signature_by_an_unknown_key_triggers_one_rediscovery_a_minute() {
    let server = MockServer::start().await;
    mount_discovery(&server, "RS256").await;
    let app = app(discovered(&server).await, "https://queue.example.org");
    let (cookie, state, nonce) = start_login(&app).await;
    token_answers(
        &server,
        jwt(
            json!({ "alg": "RS256", "typ": "JWT", "kid": "rotated" }),
            claims(&server, &nonce, vec![]),
        ),
    )
    .await;
    assert_eq!(discoveries(&server.received_requests().await.unwrap()), 1);

    for _ in 0..3 {
        let response = get(
            app.clone(),
            &format!("/auth/callback?code=c&state={state}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(
        discoveries(&server.received_requests().await.unwrap()),
        2,
        "one rediscovery for three failures"
    );
}

/// A sign-in that fails for any other reason does not bother the provider.
#[tokio::test]
async fn a_wrong_nonce_does_not_trigger_a_rediscovery() {
    let server = MockServer::start().await;
    mount_discovery(&server, "HS256").await;
    let app = app(discovered(&server).await, "https://queue.example.org");
    let (cookie, state, _nonce) = start_login(&app).await;
    token_answers(
        &server,
        jwt(
            json!({ "alg": "HS256", "typ": "JWT" }),
            claims(&server, "some-other-nonce", vec![]),
        ),
    )
    .await;
    let response = get(
        app,
        &format!("/auth/callback?code=c&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(discoveries(&server.received_requests().await.unwrap()), 1);
}
