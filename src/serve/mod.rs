//! The page people reach: sign-in through OIDC, then one read-only page
//! rendered from the collector's snapshot.
//!
//! This side holds no service API key — only the bearer token for the
//! collector, the OIDC client secret and the cookie key. What it can show is
//! bounded by [`crate::snapshot::Snapshot`]; what it shows to whom is decided
//! here: nothing without a session, nothing but a refusal without the
//! required group, and in "My requests" only the signed-in person's own.

pub mod fetch;
pub mod i18n;
pub mod oidc;
pub mod page;
pub mod session;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{FromRef, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::{Key, PrivateCookieJar};
use jiff::tz::TimeZone;

use crate::config::{ServeConfig, read_secret};
use crate::serve::fetch::SnapshotSource;
use crate::serve::i18n::{Lang, Strings};
use crate::serve::oidc::{
    Identity, Oidc, OidcSettings, Provider, REDISCOVER_EVERY, states_match, with_backoff,
};
use crate::serve::session::{
    PENDING_COOKIE, SESSION_COOKIE, Session, now_unix, pending_cookie, read_pending, read_session,
    session_cookie, spent,
};

const STYLE: &str = include_str!("style.css");

/// What the routes share. Built by [`run`] from the configuration, and by
/// the tests without a provider.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub cookie_key: Key,
    /// `None` only in tests: [`run`] does not serve before discovery
    /// succeeded. Sign-in routes answer 503 without one.
    pub provider: Option<Arc<Oidc>>,
    pub snapshots: SnapshotSource,
    pub settings: Settings,
}

/// The parts of the configuration the routes read.
pub struct Settings {
    /// Without trailing slash.
    pub public_url: String,
    pub lang: Lang,
    pub required_group: Option<String>,
    pub tz: TimeZone,
    /// The way back to the site's start page, if the operator named one.
    pub home_url: Option<String>,
}

impl Settings {
    /// Cookies are `Secure` unless the page is served over plain http — which
    /// only a test VM does, and where a `Secure` cookie would never come
    /// back.
    pub fn secure_cookies(&self) -> bool {
        !self.public_url.starts_with("http://")
    }

    fn host(&self) -> &str {
        url_host(&self.public_url)
    }

    fn strings(&self) -> &'static Strings {
        Strings::for_lang(self.lang)
    }
}

pub(crate) fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split(['/', ':']).next().unwrap_or(rest)
}

impl AppState {
    pub fn new(
        cookie_key: Key,
        provider: Option<Provider>,
        snapshots: SnapshotSource,
        settings: Settings,
    ) -> Self {
        Self(Arc::new(Inner {
            cookie_key,
            provider: provider.map(|p| Arc::new(Oidc::new(p))),
            snapshots,
            settings,
        }))
    }
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.0.cookie_key.clone()
    }
}

/// The public address, checked: http(s), a host, no path, no trailing slash.
pub fn checked_public_url(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let url = url::Url::parse(trimmed)
        .map_err(|e| anyhow::anyhow!("serve.public_url {raw:?} is not a URL: {e}"))?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        anyhow::bail!("serve.public_url must be an https:// address, not {raw:?}");
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("serve.public_url must be the bare origin, without a path: {raw:?}");
    }
    Ok(trimmed.to_string())
}

pub async fn run(config: ServeConfig) -> anyhow::Result<()> {
    let public_url = checked_public_url(&config.public_url)?;
    if public_url.starts_with("http://") {
        eprintln!(
            "queuewatch: serve.public_url is plain http; cookies are sent without `Secure`. \
             Only for a test setup."
        );
    }
    let cookie_key = session::load_cookie_key(&config.cookie_key_file)?;
    let token = read_secret(&config.snapshot_token_file)?;
    let client_secret = read_secret(&config.oidc.client_secret_file)?;
    let snapshots = SnapshotSource::new(
        config.snapshot_url.clone(),
        &token,
        Duration::from_secs(config.snapshot_max_age_secs),
    )?;

    let oidc = OidcSettings {
        issuer: config.oidc.issuer.clone(),
        client_id: config.oidc.client_id.clone(),
        client_secret,
        redirect_uri: format!("{public_url}/auth/callback"),
        user_claim: config.oidc.user_claim.clone(),
        group_claim: config.oidc.group_claim.clone(),
    };
    // Nothing is served before the provider is known: a page that is up but
    // cannot sign anyone in looks healthy and is not.
    let provider = with_backoff(
        "OIDC discovery",
        Duration::from_secs(2),
        Duration::from_secs(30),
        Duration::from_secs(300),
        || Provider::discover(&oidc),
    )
    .await?;

    let state = AppState::new(
        cookie_key,
        Some(provider),
        snapshots,
        Settings {
            public_url,
            lang: Lang::from_code(&config.language),
            required_group: config.oidc.required_group.clone().filter(|g| !g.is_empty()),
            tz: TimeZone::system(),
            home_url: config.home_url.clone(),
        },
    );

    // Keys rotate at the provider; see `Oidc` for why this runs at all.
    if let Some(oidc) = &state.0.provider {
        oidc.refresh_periodically(REDISCOVER_EVERY);
    }

    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on {}: {e}", config.listen))?;
    eprintln!("queuewatch: serving on {}", config.listen);
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(t) => t,
                Err(_) => {
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
        .route("/healthz", get(|| async { "ok" }))
        .route("/style.css", get(style))
        .fallback(not_found)
        .layer(axum::middleware::map_response(security_headers))
        .with_state(state)
}

/// On every response, whatever route produced it.
///
/// `Cache-Control: no-store` is the default and not only on HTML: every
/// answer here except the stylesheet is about one person, and a shared cache
/// must not keep it. The stylesheet sets its own header first and keeps it.
async fn security_headers(mut response: Response) -> Response {
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'self'; img-src 'self' data:; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

async fn style() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            // The page links `/style.css?v=<version>`, so a new release is a
            // new address and the old copy may be kept for a year.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        STYLE,
    )
        .into_response()
}

fn context(state: &AppState) -> page::Context<'_> {
    page::Context {
        s: state.0.settings.strings(),
        tz: &state.0.settings.tz,
        host: state.0.settings.host(),
        home: state.0.settings.home_url.as_deref(),
        now: jiff::Timestamp::now(),
    }
}

fn html_response(status: StatusCode, markup: maud::Markup) -> Response {
    (status, markup).into_response()
}

async fn not_found(State(state): State<AppState>) -> Response {
    let ctx = context(&state);
    let s = ctx.s;
    html_response(
        StatusCode::NOT_FOUND,
        page::message_page(&ctx, None, s.not_found, s.not_found, false),
    )
}

async fn index(State(state): State<AppState>, jar: PrivateCookieJar) -> Response {
    let Some(who) = read_session(&jar, now_unix()) else {
        return Redirect::to("/auth/login").into_response();
    };
    let ctx = context(&state);
    if let Some(group) = &state.0.settings.required_group
        && !who.is_member(group)
    {
        let s = ctx.s;
        return html_response(
            StatusCode::FORBIDDEN,
            page::message_page(
                &ctx,
                Some(&who.username),
                s.forbidden_title,
                s.forbidden_text,
                false,
            ),
        );
    }
    let view = state.0.snapshots.get().await;
    html_response(StatusCode::OK, page::status_page(&ctx, &who, &view))
}

fn login_trouble(state: &AppState, status: StatusCode, unavailable: bool) -> Response {
    let ctx = context(state);
    let s = ctx.s;
    let text = if unavailable {
        s.login_unavailable_text
    } else {
        s.login_failed_text
    };
    html_response(
        status,
        page::message_page(&ctx, None, s.login_failed_title, text, true),
    )
}

async fn login(State(state): State<AppState>) -> Response {
    let Some(provider) = &state.0.provider else {
        return login_trouble(&state, StatusCode::SERVICE_UNAVAILABLE, true);
    };
    let start = provider.begin_login().await;
    let jar = PrivateCookieJar::new(state.0.cookie_key.clone()).add(pending_cookie(
        &start.pending,
        state.0.settings.secure_cookies(),
    ));
    (jar, Redirect::to(&start.url)).into_response()
}

/// What the provider appends to the redirect URI.
#[derive(serde::Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// The provider can answer with a refusal instead of a code. Saying so is
    /// better than a blank page that looks like a bug here.
    error: Option<String>,
}

async fn callback(
    State(state): State<AppState>,
    jar: PrivateCookieJar,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if let Some(error) = query.error {
        // The provider's own words are not repeated back into the page; they
        // are its vocabulary, not ours. Only a short, printable excerpt goes
        // into the log.
        let excerpt: String = error
            .chars()
            .filter(|c| c.is_ascii_graphic())
            .take(64)
            .collect();
        eprintln!("queuewatch: the provider refused a sign-in: {excerpt}");
        return login_trouble(&state, StatusCode::FORBIDDEN, false);
    }

    let (Some(code), Some(returned_state)) = (query.code, query.state) else {
        return login_trouble(&state, StatusCode::BAD_REQUEST, false);
    };

    // No cookie, nothing to compare `state` against. That is a refusal and not
    // a new sign-in: silently starting one here would make an unsolicited
    // callback indistinguishable from a real one.
    let Some(pending) = read_pending(&jar) else {
        return login_trouble(&state, StatusCode::BAD_REQUEST, false);
    };
    // Checked here, before a provider is even needed, and again inside
    // `finish_login`.
    if !states_match(&pending.state, &returned_state) {
        eprintln!("queuewatch: a callback's state did not match its sign-in; refused");
        return login_trouble(&state, StatusCode::BAD_REQUEST, false);
    }

    let Some(provider) = &state.0.provider else {
        return login_trouble(&state, StatusCode::SERVICE_UNAVAILABLE, true);
    };

    let identity = match provider.finish_login(pending, &code, &returned_state).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("queuewatch: a sign-in could not be completed: {e:#}");
            return login_trouble(&state, StatusCode::FORBIDDEN, false);
        }
    };

    let session = session_from(
        identity,
        state.0.settings.required_group.as_deref(),
        now_unix(),
    );
    let jar = jar
        .remove(spent(PENDING_COOKIE, "/auth"))
        .add(session_cookie(&session, state.0.settings.secure_cookies()));
    (jar, Redirect::to("/")).into_response()
}

/// The session a verified identity becomes.
///
/// The cookie keeps only the one group that is ever asked about. All of
/// them would be dead weight, and for someone in many groups enough to push
/// the cookie past the browser's ~4 KB limit — the browser then drops it
/// silently and every visit starts a new sign-in.
fn session_from(mut identity: Identity, required_group: Option<&str>, now: i64) -> Session {
    identity
        .groups
        .retain(|g| Some(g.as_str()) == required_group);
    Session::new(identity, now)
}

/// The defence against a forged sign-out, and the first line that is ours.
///
/// The first is `SameSite=Lax` on the session cookie, which a cross-site
/// POST does not carry. That works in the browser; here the server asks
/// too (treff learned in an audit on 2026-09-15 that the server itself had
/// asked nothing).
///
/// **`Sec-Fetch-Site` first.** This page sends `Referrer-Policy: no-referrer`,
/// so there is often no referrer to read; the fetch metadata a browser
/// attaches is not something the page doing the asking can set or suppress.
/// Only `same-origin` passes. Where it is absent, `Origin` must be this
/// page's own address if present. **Neither header at all is let through:**
/// a forged request borrows a BROWSER's cookies, and every browser that can
/// be borrowed from sends at least one of them.
fn same_origin_enough(headers: &HeaderMap, public_url: &str) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site.as_bytes() == b"same-origin";
    }
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin.as_bytes() == public_url.as_bytes(),
    }
}

/// Signing out drops the cookie in this browser. A POST, because it changes
/// something: a GET would end the session for anything that merely fetches
/// a link — a chat unfurling it, a mail client building a preview.
async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: PrivateCookieJar,
) -> Response {
    if !same_origin_enough(&headers, &state.0.settings.public_url) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    let jar = jar.remove(spent(SESSION_COOKIE, "/"));
    (jar, Redirect::to("/")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_public_url_is_an_origin() {
        assert_eq!(
            checked_public_url("https://queue.example.org/").unwrap(),
            "https://queue.example.org"
        );
        assert!(checked_public_url("http://10.0.0.1:8080").is_ok());
        assert!(checked_public_url("ftp://queue.example.org").is_err());
        assert!(checked_public_url("https://queue.example.org/sub").is_err());
        assert!(checked_public_url("queue.example.org").is_err());
    }

    #[test]
    fn the_host_is_taken_from_the_public_url() {
        assert_eq!(url_host("https://queue.example.org"), "queue.example.org");
        assert_eq!(url_host("http://10.0.0.1:8080"), "10.0.0.1");
    }

    /// ONLY THE REQUIRED GROUP GOES INTO THE COOKIE — and with no group
    /// required, none at all.
    #[test]
    fn a_session_keeps_only_the_required_group() {
        let many = Identity {
            username: "ada".into(),
            email: None,
            groups: (0..300)
                .map(|i| format!("group-{i}"))
                .chain(["friends".to_string()])
                .collect(),
        };
        let s = session_from(many.clone(), Some("friends"), 0);
        assert_eq!(s.groups, ["friends"]);
        assert!(s.is_member("friends"));
        assert!(session_from(many.clone(), None, 0).groups.is_empty());
        assert!(
            session_from(many, Some("absent"), 0).groups.is_empty(),
            "not a member stays not a member"
        );
    }

    #[test]
    fn a_cross_site_sign_out_is_refused() {
        let me = "https://queue.example.org";
        let with = |pairs: &[(&'static str, &'static str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, HeaderValue::from_static(v));
            }
            same_origin_enough(&h, me)
        };
        assert!(with(&[("sec-fetch-site", "same-origin")]));
        assert!(!with(&[("sec-fetch-site", "cross-site")]));
        assert!(!with(&[("sec-fetch-site", "same-site")]));
        assert!(with(&[("origin", "https://queue.example.org")]));
        assert!(!with(&[("origin", "https://evil.example")]));
        assert!(!with(&[("origin", "null")]));
        assert!(with(&[]), "no browser, no cookies to borrow");
    }
}
