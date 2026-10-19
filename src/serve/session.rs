//! How a signed-in person is remembered between requests.
//!
//! There is no database here, so the session lives in the cookie itself:
//! a private (encrypted and authenticated) cookie holding who signed in and
//! until when. The browser can carry it but neither read nor change it, and
//! a cookie made with any other key is simply no session.
//!
//! What this costs, and why it is acceptable here: a session cannot be ended
//! from the server side before `expires_at`. Signing out drops the cookie in
//! this browser; a copy taken earlier stays valid for at most twelve hours.
//! treff, which can write, keeps its sessions in a database for exactly that
//! reason. This page only reads, and what a stolen cookie shows is a queue.

use std::path::Path;

use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar, SameSite};
use serde::{Deserialize, Serialize};

use crate::serve::oidc::{Identity, PendingLogin};

pub const SESSION_COOKIE: &str = "queuewatch_session";
pub const PENDING_COOKIE: &str = "queuewatch_pending";

/// Twelve hours, as in treff.
pub const SESSION_SECONDS: i64 = 60 * 60 * 12;

/// How long a sign-in may take between the redirect out and the callback.
const PENDING_SECONDS: i64 = 60 * 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub username: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    /// Unix seconds. Checked on every request: the cookie's own Max-Age is a
    /// request to the browser, not a guarantee.
    pub expires_at: i64,
}

impl Session {
    pub fn new(identity: Identity, now: i64) -> Self {
        Self {
            username: identity.username,
            email: identity.email,
            groups: identity.groups,
            expires_at: now + SESSION_SECONDS,
        }
    }

    /// Exact, case-sensitive: a group name is an identifier at the provider,
    /// and "friends" is not "Friends" there either.
    pub fn is_member(&self, group: &str) -> bool {
        self.groups.iter().any(|g| g == group)
    }
}

pub fn now_unix() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// The cookie that carries a session. Same function in production and in the
/// tests, so a test that gets in proves a browser would.
pub fn session_cookie(session: &Session, secure: bool) -> Cookie<'static> {
    // Serialising a struct of strings cannot fail; should it ever, the
    // result is an empty cookie, which is no session — fail closed.
    let value = serde_json::to_string(session).unwrap_or_default();
    let mut c = Cookie::new(SESSION_COOKIE, value);
    c.set_http_only(true);
    c.set_secure(secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    c.set_max_age(time::Duration::seconds(SESSION_SECONDS));
    c
}

/// The session in this request, if there is one that is still valid.
/// Undecryptable, unparsable and expired all mean the same: none.
pub fn read_session(jar: &PrivateCookieJar, now: i64) -> Option<Session> {
    let cookie = jar.get(SESSION_COOKIE)?;
    let session: Session = serde_json::from_str(cookie.value()).ok()?;
    (session.expires_at > now && !session.username.is_empty()).then_some(session)
}

/// state, nonce and the PKCE verifier travel in one short-lived private
/// cookie, scoped to `/auth`. They never appear in a URL.
pub fn pending_cookie(pending: &PendingLogin, secure: bool) -> Cookie<'static> {
    let value = serde_json::to_string(pending).unwrap_or_default();
    let mut c = Cookie::new(PENDING_COOKIE, value);
    c.set_http_only(true);
    c.set_secure(secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/auth");
    c.set_max_age(time::Duration::seconds(PENDING_SECONDS));
    c
}

pub fn read_pending(jar: &PrivateCookieJar) -> Option<PendingLogin> {
    let cookie = jar.get(PENDING_COOKIE)?;
    let p: PendingLogin = serde_json::from_str(cookie.value()).ok()?;
    (!p.state.is_empty() && !p.nonce.is_empty() && !p.pkce_verifier.is_empty()).then_some(p)
}

/// A removal needs the same path the cookie was set with, or the browser
/// keeps a second one alongside.
pub fn spent(name: &'static str, path: &'static str) -> Cookie<'static> {
    let mut c = Cookie::new(name, "");
    c.set_path(path);
    c
}

/// The key of the private cookies, from its file.
///
/// The file holds 64 bytes or more — raw, or as base64 text (what
/// `openssl rand -base64 64` writes). Shorter is refused, not stretched: a
/// short key stretched by a derivation is still only as hard to guess as it
/// was short. And there is no generated fallback, as treff has: a key made up
/// at start would sign everybody out at every restart, and a key nobody
/// configured is a deployment mistake to report, not to paper over.
pub fn load_cookie_key(path: &Path) -> anyhow::Result<Key> {
    let bytes =
        std::fs::read(path).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    parse_cookie_key(&bytes).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

pub fn parse_cookie_key(bytes: &[u8]) -> anyhow::Result<Key> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD_PAD_INDIFFERENT, URL_SAFE_PAD_INDIFFERENT};

    // Base64 first: a text file that decodes is meant as base64 — its raw
    // bytes would be a key drawn from 64 of 256 possible values per byte.
    let decoded = std::str::from_utf8(bytes).ok().and_then(|text| {
        // Line breaks are allowed: `openssl rand -base64 64` wraps at 64
        // characters.
        let compact: String = text.split_whitespace().collect();
        if compact.is_empty() {
            return None;
        }
        STANDARD_PAD_INDIFFERENT
            .decode(&compact)
            .or_else(|_| URL_SAFE_PAD_INDIFFERENT.decode(&compact))
            .ok()
    });
    let key = decoded.unwrap_or_else(|| bytes.to_vec());
    if key.len() < 64 {
        anyhow::bail!(
            "the cookie key has {} bytes; it needs at least 64 (e.g. `openssl rand -base64 64`)",
            key.len()
        );
    }
    Key::try_from(key.as_slice()).map_err(|e| anyhow::anyhow!("unusable cookie key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;

    fn key() -> Key {
        Key::from(&[7u8; 64])
    }

    /// The cookie as a browser would send it back: what the jar set, sent
    /// as a `Cookie` header.
    fn round_trip(jar: PrivateCookieJar, key: Key) -> PrivateCookieJar {
        let response = jar.into_response();
        let mut headers = HeaderMap::new();
        let pairs: Vec<String> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
            .collect();
        headers.insert("cookie", pairs.join("; ").parse().unwrap());
        PrivateCookieJar::from_headers(&headers, key)
    }

    fn ada(now: i64) -> Session {
        Session::new(
            Identity {
                username: "ada".into(),
                email: Some("ada@example.org".into()),
                groups: vec!["Friends".into()],
            },
            now,
        )
    }

    #[test]
    fn a_session_round_trips_through_the_cookie() {
        let s = ada(1000);
        let jar = PrivateCookieJar::new(key()).add(session_cookie(&s, true));
        let back = round_trip(jar, key());
        assert_eq!(read_session(&back, 1001), Some(s));
    }

    #[test]
    fn an_expired_session_is_none() {
        let s = ada(1000);
        let jar = round_trip(
            PrivateCookieJar::new(key()).add(session_cookie(&s, true)),
            key(),
        );
        assert_eq!(read_session(&jar, 1000 + SESSION_SECONDS), None);
    }

    #[test]
    fn a_cookie_made_with_another_key_is_no_session() {
        let s = ada(1000);
        let jar = PrivateCookieJar::new(Key::from(&[9u8; 64])).add(session_cookie(&s, true));
        let back = round_trip(jar, key());
        assert_eq!(read_session(&back, 1001), None);
    }

    #[test]
    fn the_value_is_not_readable_in_the_browser() {
        let s = ada(1000);
        let response = PrivateCookieJar::new(key())
            .add(session_cookie(&s, true))
            .into_response();
        let set = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(!set.contains("ada"), "{set}");
        for attr in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
            assert!(set.contains(attr), "{attr} missing: {set}");
        }
    }

    #[test]
    fn membership_is_exact() {
        let s = ada(0);
        assert!(s.is_member("Friends"));
        assert!(!s.is_member("friends"));
        assert!(!s.is_member(""));
    }

    #[test]
    fn a_pending_login_round_trips_and_an_incomplete_one_does_not() {
        let p = PendingLogin {
            state: "s".into(),
            nonce: "n".into(),
            pkce_verifier: "v".into(),
        };
        let jar = round_trip(
            PrivateCookieJar::new(key()).add(pending_cookie(&p, true)),
            key(),
        );
        assert_eq!(read_pending(&jar), Some(p.clone()));

        let empty = PendingLogin {
            state: String::new(),
            ..p
        };
        let jar = round_trip(
            PrivateCookieJar::new(key()).add(pending_cookie(&empty, true)),
            key(),
        );
        assert_eq!(read_pending(&jar), None, "an empty state is not a wildcard");
    }

    #[test]
    fn a_short_key_is_refused_raw_and_as_base64() {
        use base64::Engine as _;
        assert!(parse_cookie_key(&[1u8; 63]).is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 48]);
        // 64 characters of text, but only 48 bytes of key.
        assert_eq!(short.len(), 64);
        assert!(parse_cookie_key(short.as_bytes()).is_err());
        assert!(parse_cookie_key(b"").is_err());
    }

    #[test]
    fn a_long_enough_key_is_accepted_raw_and_as_wrapped_base64() {
        use base64::Engine as _;
        assert!(parse_cookie_key(&[200u8; 64]).is_ok());
        let text = base64::engine::general_purpose::STANDARD.encode([3u8; 64]);
        let wrapped = format!("{}\n{}\n", &text[..64], &text[64..]);
        assert!(parse_cookie_key(wrapped.as_bytes()).is_ok());
    }

    #[test]
    fn base64_text_and_its_raw_bytes_are_different_keys() {
        // The decoded key is used, not the text: a cookie made with the
        // decoded bytes must open with the key read from the text file.
        use base64::Engine as _;
        let raw = [5u8; 64];
        let from_text = parse_cookie_key(
            base64::engine::general_purpose::STANDARD
                .encode(raw)
                .as_bytes(),
        )
        .expect("key");
        let s = ada(1000);
        let jar = PrivateCookieJar::new(Key::from(&raw)).add(session_cookie(&s, true));
        assert_eq!(read_session(&round_trip(jar, from_text), 1001), Some(s));
    }
}
