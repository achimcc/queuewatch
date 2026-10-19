//! The OIDC conversation: discovery at startup, the redirect out, the
//! callback back — and discovery again when the provider's signing keys may
//! have changed. Adapted from treff (same author, same licence).
//!
//! What this module does **not** do is re-implement token verification.
//! Checking the ID token's signature against the provider's keys, and checking
//! the nonce, is `openidconnect`'s job; imitating it here with home-made
//! fixtures would test the library rather than this code. What is tested here
//! is what we decide: that the outgoing request carries PKCE, `state` and
//! `nonce`, that a callback whose `state` does not match is refused before
//! anything else happens, and how a set of claims becomes an identity.
//!
//! **Why not trust a header or a JWT from a forward-auth proxy.** The page is
//! meant to run behind any provider — Authentik, Keycloak, Zitadel,
//! Pocket-ID — and without a proxy that adds headers at all. A proxy door in
//! front of it is welcome, but it is a second layer: the identity and the
//! groups come from the ID token this process verified itself, so a fault in
//! either layer does not open the page.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClaimsVerificationError, ClientId, ClientSecret, CsrfToken,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use tokio::time::Instant;

/// The client type after `set_redirect_uri`. The endpoint markers are part of
/// the type in openidconnect 4, so storing a configured client means naming
/// them.
type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// What the provider needs to know about us, resolved from the
/// configuration (the secret already read from its file). Kept by the
/// [`Provider`], because every later discovery needs it again.
#[derive(Clone)]
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// `<public_url>/auth/callback`.
    pub redirect_uri: String,
    pub user_claim: String,
    pub group_claim: String,
}

/// Who signed in, as far as this page cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// From `oidc.user_claim`; matched against the request manager's user
    /// name.
    pub username: String,
    /// Only an address the provider marked `email_verified`.
    pub email: Option<String>,
    pub groups: Vec<String>,
}

pub struct Provider {
    client: Client,
    http: openidconnect::reqwest::Client,
    settings: OidcSettings,
}

/// What a sign-in needs to remember between the redirect out and the callback
/// back. It travels in a short-lived private cookie, never in a URL.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingLogin {
    pub state: String,
    pub nonce: String,
    pub pkce_verifier: String,
}

pub struct LoginStart {
    pub url: String,
    pub pending: PendingLogin,
}

/// Compares two `state` values without an early exit.
///
/// An empty stored state is **not** a wildcard: a callback that arrives
/// without a login having started is refused, which is what makes this a CSRF
/// defence rather than a formality.
pub fn states_match(stored: &str, returned: &str) -> bool {
    if stored.is_empty() || stored.len() != returned.len() {
        return false;
    }
    stored
        .bytes()
        .zip(returned.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Claims in, identity out.
///
/// The person's name is required: it is what their requests are found by,
/// and a sign-in without it would be a page that silently shows nobody's
/// requests — a provider misconfiguration that looks like an empty list. So a
/// missing or non-string claim refuses the sign-in, with a message that names
/// the claim.
///
/// Everything else unexpected results in **nothing**: a missing group claim,
/// a claim that is a string rather than a list, entries that are not strings
/// all mean no groups; an address that is not a string is no address. The
/// alternative — guessing — would turn a provider's misconfiguration into an
/// open door.
pub fn claims_to_identity(
    claims: &serde_json::Value,
    user_claim: &str,
    group_claim: &str,
) -> anyhow::Result<Identity> {
    let username = claims
        .get(user_claim)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("the ID token carries no string claim {user_claim:?} (oidc.user_claim)")
        })?
        .to_string();

    let groups = claims
        .get(group_claim)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|g| g.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // AN ADDRESS COUNTS ONLY WHEN THE PROVIDER VOUCHES FOR IT. "My requests"
    // matches by address as well as by name, so an unverified address is a
    // way in: register at the provider with somebody else's address (many
    // let you type any) and their requests are yours to read. Only the JSON
    // boolean `true` counts — absent, `false` and the string "true" are all
    // "not verified"; guessing the generous reading is how doors open.
    let verified = claims.get("email_verified").and_then(|v| v.as_bool()) == Some(true);
    let email = claims
        .get("email")
        .filter(|_| verified)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(String::from);

    Ok(Identity {
        username,
        email,
        groups,
    })
}

/// Runs `attempt` until it succeeds, waiting `first`, then twice as long each
/// time up to `cap`, and gives up once `deadline` has passed.
///
/// WHY RETRY AT ALL, when treff discovers once and fails: a deploy restarts
/// the provider and this page together, and a page that asked once, while
/// the provider was still starting, and then exited (or worse, ran on
/// without sign-in) is broken until somebody notices. Giving up after a
/// bounded time is still the point — a provider that never answers is a
/// configuration error, and it belongs in the unit's status, not in an
/// endless loop that looks like a running service.
pub async fn with_backoff<T, F, Fut>(
    what: &str,
    first: Duration,
    cap: Duration,
    deadline: Duration,
    mut attempt: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let started = tokio::time::Instant::now();
    let mut wait = first;
    loop {
        match attempt().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if started.elapsed() + wait > deadline {
                    return Err(e.context(format!(
                        "{what} still failing after {} s; giving up",
                        started.elapsed().as_secs()
                    )));
                }
                eprintln!(
                    "queuewatch: {what} failed ({e:#}); retrying in {} s",
                    wait.as_secs_f32()
                );
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(cap);
            }
        }
    }
}

impl Provider {
    /// One discovery attempt. See [`with_backoff`] for why the caller
    /// retries.
    pub async fn discover(settings: &OidcSettings) -> anyhow::Result<Self> {
        // An empty secret is refused here as well as when the file is read:
        // a provider with a client that has no secret answers every token
        // request with an error that looks like a bug in this page.
        if settings.client_secret.trim().is_empty() {
            anyhow::bail!("the OIDC client secret is empty; queuewatch does not start without it");
        }

        // Following redirects here would open the door to SSRF, so the client
        // does not follow any. The timeout keeps a provider that accepts the
        // connection and never answers from hanging a sign-in forever.
        let http = openidconnect::reqwest::ClientBuilder::new()
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;

        let metadata =
            CoreProviderMetadata::discover_async(IssuerUrl::new(settings.issuer.clone())?, &http)
                .await?;

        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(settings.client_id.clone()),
            Some(ClientSecret::new(settings.client_secret.clone())),
        )
        .set_redirect_uri(RedirectUrl::new(settings.redirect_uri.clone())?);

        Ok(Self {
            client,
            http,
            settings: settings.clone(),
        })
    }

    pub fn begin_login(&self) -> LoginStart {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, csrf, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("profile".to_string()))
            // `email` is asked for and NOT required. A provider that does not
            // offer it refuses the request loudly, which is a configuration
            // problem with a message; a claim that is quietly absent is one
            // without. Requests are matched by address only as a second way.
            .add_scope(Scope::new("email".to_string()))
            .set_pkce_challenge(challenge)
            .url();

        LoginStart {
            url: url.to_string(),
            pending: PendingLogin {
                state: csrf.into_secret(),
                nonce: nonce.secret().clone(),
                pkce_verifier: verifier.into_secret(),
            },
        }
    }

    /// Exchanges the code for tokens and turns the ID token's claims into an
    /// identity.
    ///
    /// The `state` is checked **first**, before the provider is contacted at
    /// all: a callback that nobody started is not worth a network request.
    /// (The route checks it too, before it even needs a provider; this check
    /// stays so that the function is safe on its own.)
    pub async fn finish_login(
        &self,
        pending: PendingLogin,
        code: &str,
        returned_state: &str,
    ) -> anyhow::Result<Identity> {
        if !states_match(&pending.state, returned_state) {
            anyhow::bail!("the callback state does not match the one this sign-in started with");
        }

        let tokens = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))?
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier))
            .request_async(&self.http)
            .await?;

        let id_token = tokens
            .id_token()
            .ok_or_else(|| anyhow::anyhow!("the provider returned no ID token"))?;
        // Signature, issuer, audience, expiry and nonce.
        id_token.claims(&self.client.id_token_verifier(), &Nonce::new(pending.nonce))?;

        // THE CLAIMS ARE READ BACK FROM THE TOKEN THAT WAS JUST VERIFIED,
        // one line above. `claims.additional_claims()` cannot carry them:
        // `CoreClient` is `EmptyAdditionalClaims`, so that call returns `{}`
        // whatever the provider sent. In treff on 2026-09-06 that meant
        // everybody signed in successfully and was then refused — `groups: []`
        // in the session — with a provider that had the claim configured
        // correctly and a unit test for the claim mapping that passed, because
        // it was handed a hand-written JSON value rather than what this path
        // produces. The user claim is configurable here, so it goes the same
        // way.
        //
        // ORDER IS THE SAFETY PROPERTY HERE: signature, issuer, audience and
        // nonce are checked by `claims()` above; this only re-reads the same
        // bytes. Moving it before that line would turn a verified token into
        // an unverified one, and nothing in the type system says so.
        let claims = verified_payload(&id_token.to_string())?;
        claims_to_identity(
            &claims,
            &self.settings.user_claim,
            &self.settings.group_claim,
        )
    }
}

/// How soon after one discovery a failed sign-in may cause the next.
pub const REDISCOVER_AFTER_KEY_ERROR: Duration = Duration::from_secs(60);

/// How often discovery runs anyway, without a failure to prompt it.
pub const REDISCOVER_EVERY: Duration = Duration::from_secs(60 * 60);

/// Did this sign-in fail on the ID token's signature — an unknown key id, a
/// key that no longer verifies, an algorithm we were not told about? Those
/// are what a key rotation at the provider looks like from here. Anything
/// else (a wrong nonce, an expired token, a refused code) is not cured by
/// asking the provider again.
pub fn is_key_error(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ClaimsVerificationError>(),
        Some(ClaimsVerificationError::SignatureVerification(_))
    )
}

/// "At most once per `gap`", for the rediscovery a failed sign-in triggers.
///
/// WHY A GATE: the failure that prompts a discovery is one anybody can cause
/// — a forged callback with a token signed by nobody's key costs a GET of
/// the callback URL. Without a bound, every such request would be two
/// requests at the provider, and the page a tool to load it.
#[derive(Debug, Default)]
pub struct RefreshGate {
    last: Option<Instant>,
}

impl RefreshGate {
    /// True, and remembers `now`, when the last claim is at least `gap` ago.
    pub fn claim(&mut self, now: Instant, gap: Duration) -> bool {
        match self.last {
            Some(last) if now.saturating_duration_since(last) < gap => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// The provider as the routes use it: swapped for a freshly discovered one
/// when its keys may have changed.
///
/// WHY: discovery also fetches the provider's signing keys (JWKS), and before
/// this the page did that once, at start. A provider that rotates its keys —
/// Authentik on a new certificate, Keycloak on a scheduled rotation — then
/// signs tokens this process cannot verify, and every sign-in fails until
/// somebody restarts the page. So discovery runs again after a sign-in fails
/// on the signature (at most once a minute, see [`RefreshGate`]) and every
/// hour regardless, so that most rotations are picked up before anyone hits
/// them. A discovery that fails keeps the provider there is: old keys are
/// better than none.
pub struct Oidc {
    current: tokio::sync::RwLock<Arc<Provider>>,
    gate: Mutex<RefreshGate>,
}

impl Oidc {
    pub fn new(provider: Provider) -> Self {
        Self {
            current: tokio::sync::RwLock::new(Arc::new(provider)),
            gate: Mutex::new(RefreshGate::default()),
        }
    }

    /// The provider of the moment. Cloned out of the lock, so that no sign-in
    /// holds it across a request to the provider and a swap never waits.
    pub async fn current(&self) -> Arc<Provider> {
        Arc::clone(&*self.current.read().await)
    }

    pub async fn begin_login(&self) -> LoginStart {
        self.current().await.begin_login()
    }

    /// [`Provider::finish_login`], and a rediscovery when it failed on the
    /// signature. The failed sign-in stays failed — its code is spent — but
    /// the person's "try again" then meets the new keys.
    pub async fn finish_login(
        &self,
        pending: PendingLogin,
        code: &str,
        returned_state: &str,
    ) -> anyhow::Result<Identity> {
        let result = self
            .current()
            .await
            .finish_login(pending, code, returned_state)
            .await;
        if let Err(e) = &result
            && is_key_error(e)
            && self
                .gate
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .claim(Instant::now(), REDISCOVER_AFTER_KEY_ERROR)
        {
            eprintln!("queuewatch: a sign-in failed on its signature; asking the provider again");
            if let Err(e) = self.refresh().await {
                eprintln!("queuewatch: OIDC rediscovery failed ({e:#}); keeping the old keys");
            }
        }
        result
    }

    /// One discovery with the settings of the current provider; on success
    /// it replaces the current provider.
    pub async fn refresh(&self) -> anyhow::Result<()> {
        let settings = self.current().await.settings.clone();
        let fresh = Provider::discover(&settings).await?;
        *self.current.write().await = Arc::new(fresh);
        Ok(())
    }

    /// Discovery every `every`, for as long as the page runs.
    pub fn refresh_periodically(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let oidc = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval_at(Instant::now() + every, every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = oidc.refresh().await {
                    eprintln!(
                        "queuewatch: periodic OIDC discovery failed ({e:#}); keeping the old keys"
                    );
                }
            }
        })
    }
}

/// The JWT payload as plain JSON.
///
/// Only ever called on a token whose signature has already been checked — see
/// the comment at the call site. A JWT is three dot-separated base64url
/// segments; the middle one is the claim set.
fn verified_payload(jwt: &str) -> anyhow::Result<serde_json::Value> {
    use base64::Engine as _;
    let mut parts = jwt.split('.');
    let (_header, payload, signature) = (
        parts.next(),
        parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("the ID token has no payload"))?,
        parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("the ID token has no signature"))?,
    );
    if signature.is_empty() || parts.next().is_some() {
        anyhow::bail!("the ID token is not a three-part JWT");
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    const REDIRECT: &str = "https://queue.example.org/auth/callback";

    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn settings(issuer: String) -> OidcSettings {
        OidcSettings {
            issuer,
            client_id: "queuewatch".into(),
            client_secret: "s3cret".into(),
            redirect_uri: REDIRECT.into(),
            user_claim: "preferred_username".into(),
            group_claim: "groups".into(),
        }
    }

    async fn mount_discovery(server: &MockServer) {
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

        // Discovery fetches the signing keys in the same breath; without this
        // second stub the call fails with a 404 that looks like a bad URL.
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "keys": [] })))
            .mount(server)
            .await;
    }

    async fn provider_at(server: &MockServer) -> Provider {
        mount_discovery(server).await;
        Provider::discover(&settings(server.uri()))
            .await
            .expect("discovery")
    }

    /// The claims that decide everything must survive the trip out of the
    /// token. This is the step that was missing in treff: `additional_claims()`
    /// on a `CoreClient` is empty by construction.
    #[test]
    fn the_claims_survive_the_trip_out_of_the_token() {
        use base64::Engine as _;
        let payload = json!({
            "sub": "df55fb1b",
            "preferred_username": "ada",
            "email": "ada@example.org",
            "email_verified": true,
            "groups": ["Friends", "Media"],
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).expect("json"));
        let jwt = format!("aGVhZGVy.{encoded}.c2ln");

        let claims = verified_payload(&jwt).expect("payload");
        let identity = claims_to_identity(&claims, "preferred_username", "groups").expect("id");
        assert_eq!(identity.username, "ada");
        assert_eq!(identity.email.as_deref(), Some("ada@example.org"));
        assert_eq!(identity.groups, vec!["Friends", "Media"]);
    }

    #[test]
    fn something_that_is_not_a_jwt_is_an_error_not_an_empty_claim_set() {
        // An empty claim set would mean no groups, which reads exactly like a
        // person who belongs to none — a refusal nobody can explain.
        for wrong in ["", "one.two", "a.b.c.d", "header..sig"] {
            assert!(verified_payload(wrong).is_err(), "{wrong:?} was accepted");
        }
    }

    #[test]
    fn a_missing_user_claim_refuses_the_sign_in_and_names_the_claim() {
        let err = claims_to_identity(&json!({ "sub": "x" }), "preferred_username", "groups")
            .expect_err("must refuse");
        assert!(err.to_string().contains("preferred_username"), "{err}");
        assert!(
            claims_to_identity(
                &json!({ "preferred_username": 7 }),
                "preferred_username",
                "groups"
            )
            .is_err()
        );
        assert!(
            claims_to_identity(
                &json!({ "preferred_username": "  " }),
                "preferred_username",
                "groups"
            )
            .is_err()
        );
    }

    #[test]
    fn a_different_user_claim_is_honoured() {
        let id =
            claims_to_identity(&json!({ "nickname": " Ada " }), "nickname", "groups").expect("id");
        assert_eq!(id.username, "Ada", "trimmed, case kept");
    }

    #[test]
    fn a_missing_group_claim_means_no_groups_not_all_groups() {
        let id = claims_to_identity(&json!({ "u": "a" }), "u", "groups").expect("id");
        assert!(id.groups.is_empty());
    }

    #[test]
    fn a_group_claim_of_the_wrong_shape_means_no_groups() {
        // A string instead of a list is a misconfiguration at the provider.
        // It must let nobody in.
        let id = claims_to_identity(&json!({ "u": "a", "groups": "Friends" }), "u", "groups")
            .expect("id");
        assert!(id.groups.is_empty());
    }

    #[test]
    fn non_string_groups_are_dropped_not_stringified() {
        let id = claims_to_identity(
            &json!({ "u": "a", "groups": ["A", 7, null] }),
            "u",
            "groups",
        )
        .expect("id");
        assert_eq!(id.groups, vec!["A"]);
    }

    #[test]
    fn a_different_group_claim_is_honoured() {
        let id = claims_to_identity(&json!({ "u": "a", "roles": ["Friends"] }), "u", "roles")
            .expect("id");
        assert_eq!(id.groups, vec!["Friends"]);
    }

    /// AN ADDRESS IS OPTIONAL. A provider that sends none is one whose people
    /// are matched by name only — not one whose people are locked out.
    #[test]
    fn an_address_is_read_when_there_is_one_and_missed_without_complaint() {
        let without = claims_to_identity(&json!({ "u": "a" }), "u", "g").expect("id");
        assert_eq!(without.email, None);
        let wrong = claims_to_identity(&json!({ "u": "a", "email": 42 }), "u", "g").expect("id");
        assert_eq!(
            wrong.email, None,
            "a claim that is not a string is not an address"
        );
    }

    /// AN ADDRESS NOBODY VERIFIED IS NO ADDRESS: it would match somebody
    /// else's requests by an address typed in at the provider.
    #[test]
    fn only_a_verified_address_is_taken() {
        let email = |extra: serde_json::Value| {
            let mut claims = json!({ "u": "a", "email": "ada@example.org" });
            if let (Some(c), Some(e)) = (claims.as_object_mut(), extra.as_object()) {
                c.extend(e.clone());
            }
            claims_to_identity(&claims, "u", "g").expect("id").email
        };
        assert_eq!(email(json!({})), None, "absent is not verified");
        assert_eq!(email(json!({ "email_verified": false })), None);
        assert_eq!(email(json!({ "email_verified": "true" })), None);
        assert_eq!(email(json!({ "email_verified": 1 })), None);
        assert_eq!(
            email(json!({ "email_verified": true })).as_deref(),
            Some("ada@example.org")
        );
    }

    #[tokio::test]
    async fn an_empty_client_secret_is_refused_before_any_request() {
        let mut s = settings("http://127.0.0.1:9".into());
        s.client_secret = "  ".into();
        let err = Provider::discover(&s).await.err().expect("must refuse");
        assert!(err.to_string().contains("empty"), "{err}");
    }

    /// The address only arrives if it is asked for. `openid profile` does not
    /// carry `email` at any provider that follows the spec.
    #[tokio::test]
    async fn the_login_asks_for_an_address() {
        let server = MockServer::start().await;
        let provider = provider_at(&server).await;
        let start = provider.begin_login();
        assert!(start.url.contains("email"), "{}", start.url);
    }

    #[tokio::test]
    async fn the_login_url_carries_pkce_state_and_nonce() {
        let server = MockServer::start().await;
        let provider = provider_at(&server).await;

        let start = provider.begin_login();
        let url = start.url;

        assert!(
            url.starts_with(&format!("{}/authorize", server.uri())),
            "{url}"
        );
        for expected in [
            "code_challenge_method=S256",
            "code_challenge=",
            "state=",
            "nonce=",
            "client_id=queuewatch",
            "response_type=code",
            "scope=openid+profile",
        ] {
            assert!(url.contains(expected), "{expected} missing from {url}");
        }
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Fqueue.example.org%2Fauth%2Fcallback"),
            "{url}"
        );
        // The verifier is the half that must NOT travel.
        assert!(!url.contains(start.pending.pkce_verifier.as_str()), "{url}");
    }

    #[tokio::test]
    async fn two_logins_do_not_share_their_secrets() {
        let server = MockServer::start().await;
        let provider = provider_at(&server).await;
        let a = provider.begin_login();
        let b = provider.begin_login();
        assert_ne!(a.pending.state, b.pending.state);
        assert_ne!(a.pending.nonce, b.pending.nonce);
        assert_ne!(a.pending.pkce_verifier, b.pending.pkce_verifier);
    }

    #[tokio::test]
    async fn a_callback_with_the_wrong_state_is_refused_without_asking_the_provider() {
        // The token endpoint is deliberately NOT mocked: if the state check
        // did not come first, this test would fail on a connection error or
        // a 404 instead of the assertion below.
        let server = MockServer::start().await;
        let provider = provider_at(&server).await;
        let start = provider.begin_login();

        let err = provider
            .finish_login(start.pending, "any-code", "not-the-state")
            .await
            .expect_err("must refuse");
        assert!(
            err.to_string().contains("state"),
            "refused for the wrong reason: {err}"
        );
    }

    #[test]
    fn the_gate_lets_one_rediscovery_through_per_minute() {
        let start = Instant::now();
        let gap = REDISCOVER_AFTER_KEY_ERROR;
        let mut gate = RefreshGate::default();
        assert!(gate.claim(start, gap), "the first failure asks");
        assert!(!gate.claim(start, gap));
        assert!(!gate.claim(start + Duration::from_secs(59), gap));
        assert!(gate.claim(start + Duration::from_secs(60), gap));
        assert!(
            !gate.claim(start + Duration::from_secs(61), gap),
            "measured from the last claim, not the first"
        );
        assert!(gate.claim(start + Duration::from_secs(125), gap));
    }

    #[test]
    fn only_a_signature_failure_counts_as_a_key_error() {
        use openidconnect::SignatureVerificationError;
        let key = |e: SignatureVerificationError| {
            is_key_error(&ClaimsVerificationError::SignatureVerification(e).into())
        };
        assert!(key(SignatureVerificationError::NoMatchingKey));
        assert!(key(SignatureVerificationError::CryptoError("x".into())));
        assert!(!is_key_error(
            &ClaimsVerificationError::InvalidNonce("x".into()).into()
        ));
        assert!(!is_key_error(
            &ClaimsVerificationError::Expired("x".into()).into()
        ));
        assert!(!is_key_error(&anyhow::anyhow!(
            "the provider returned no ID token"
        )));
    }

    /// A REFRESH SWAPS THE PROVIDER: what discovery says the second time is
    /// what the next sign-in uses.
    #[tokio::test]
    async fn a_refresh_replaces_the_provider_with_what_discovery_says_now() {
        let server = MockServer::start().await;
        let oidc = Oidc::new(provider_at(&server).await);
        assert!(oidc.begin_login().await.url.contains("/authorize?"));

        server.reset().await;
        let issuer = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize-v2"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/jwks"),
                "response_types_supported": ["code"],
                "subject_types_supported": ["public"],
                "id_token_signing_alg_values_supported": ["RS256"],
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "keys": [] })))
            .mount(&server)
            .await;
        oidc.refresh().await.expect("refresh");
        assert!(oidc.begin_login().await.url.contains("/authorize-v2?"));
    }

    /// AND A FAILED REFRESH KEEPS WHAT THERE WAS: a provider that is down
    /// for a minute must not take the page's sign-in with it.
    #[tokio::test]
    async fn a_failed_refresh_keeps_the_old_provider() {
        let server = MockServer::start().await;
        let oidc = Oidc::new(provider_at(&server).await);
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        assert!(oidc.refresh().await.is_err());
        assert!(oidc.begin_login().await.url.contains("/authorize?"));
    }

    #[test]
    fn state_comparison_rejects_near_misses() {
        assert!(states_match("abc", "abc"));
        assert!(!states_match("abc", "abd"));
        assert!(!states_match("abc", "abc "));
        assert!(!states_match("abc", "ab"));
        assert!(!states_match("", "abc"));
        // An empty stored state must never be a wildcard.
        assert!(!states_match("", ""));
    }

    /// A PROVIDER THAT IS STILL STARTING IS WAITED FOR. The first two
    /// discovery requests fail the way a restarting proxy answers; the third
    /// succeeds, and so does the whole call.
    #[tokio::test]
    async fn discovery_waits_for_a_provider_that_is_still_starting() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&server)
            .await;
        mount_discovery(&server).await;

        let s = settings(server.uri());
        let provider = with_backoff(
            "discovery",
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_secs(5),
            || Provider::discover(&s),
        )
        .await
        .expect("discovered on the third attempt");
        assert!(provider.begin_login().url.contains("state="));
    }

    /// AND ONE THAT NEVER COMES IS AN ERROR, not an endless loop.
    #[tokio::test]
    async fn discovery_gives_up_after_its_deadline() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let s = settings(server.uri());
        let err = with_backoff(
            "discovery",
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(100),
            || Provider::discover(&s),
        )
        .await
        .err()
        .expect("must give up");
        assert!(err.to_string().contains("giving up"), "{err:#}");
    }
}
