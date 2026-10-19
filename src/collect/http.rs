//! HTTP towards the services, and the one place that decides what a failure
//! may say about itself.
//!
//! A `reqwest::Error` prints the URL it failed on, and SABnzbd wants its API
//! key in the query string. So an error never leaves this module as the
//! error itself: it becomes a [`FetchError`], a class with nothing in it but
//! a status code or a position — no URL, no body, no message from the far
//! side.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use reqwest::header::{COOKIE, HeaderMap, SET_COOKIE};
use reqwest::{Client, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use url::Url;

/// Why a source did not answer. Printed on stderr, one line per failure, so
/// it is built to be safe to print: a class, never a URL or a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    Timeout,
    Connect,
    /// A non-2xx answer. 3xx included: redirects are not followed, a login
    /// page answering instead of the API is a failure, not data.
    Status(u16),
    /// The body was not what the API describes. Line and column help find
    /// the field without printing the value (serde's own message would).
    Decode {
        line: usize,
        column: usize,
    },
    /// The service answered 200 and still said no (qBittorrent's `Fails.`,
    /// SABnzbd's `{"status": false}`).
    Refused,
    /// The body was larger than [`MAX_BODY`].
    TooLarge,
    /// Anything else reqwest can fail on (building the request, the body
    /// stream breaking off).
    Request,
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Timeout => f.write_str("timeout"),
            FetchError::Connect => f.write_str("connect"),
            FetchError::Status(code) => write!(f, "status {code}"),
            FetchError::Decode { line, column } => {
                write!(f, "decode (line {line}, column {column})")
            }
            FetchError::Refused => f.write_str("refused"),
            FetchError::TooLarge => f.write_str("body too large"),
            FetchError::Request => f.write_str("request"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Reduces a reqwest error to its class. The error itself — which carries
/// the URL — is dropped here and goes nowhere else.
fn classify(e: &reqwest::Error) -> FetchError {
    if e.is_timeout() {
        FetchError::Timeout
    } else if e.is_connect() {
        FetchError::Connect
    } else if e.is_decode() {
        FetchError::Decode { line: 0, column: 0 }
    } else if let Some(status) = e.status() {
        FetchError::Status(status.as_u16())
    } else {
        FetchError::Request
    }
}

/// The client every source shares. No redirects: an API that redirects is
/// sending us to a login page, and following it would hand our cookies and
/// headers to wherever it points.
pub fn client(timeout: Duration) -> anyhow::Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .connect_timeout(timeout)
        .user_agent(concat!("queuewatch/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| anyhow::anyhow!("cannot build the HTTP client"))
}

/// The most any one answer may be. A queue of a thousand entries with every
/// field the *arr apps send is a few megabytes; this is ten times that and
/// more. The point is not a tight fit but a bound: a service that streams
/// without end (a misrouted URL, a broken proxy, a log file served by
/// mistake) must cost one source, not the collector's memory — and with it
/// every other source.
pub const MAX_BODY: usize = 32 * 1024 * 1024;

pub async fn send(request: RequestBuilder) -> Result<Response, FetchError> {
    request.send().await.map_err(|e| classify(&e))
}

/// The body, read chunk by chunk and given up on as soon as it passes
/// `limit`. `Response::bytes()` and `text()` would buffer whatever arrives;
/// every body a source reads goes through here instead.
pub async fn body_limited(mut response: Response, limit: usize) -> Result<Vec<u8>, FetchError> {
    // A declared length is believed when it says "too much" — no need to
    // read 32 MiB to learn that — but not when it says "little": it may lie.
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(FetchError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| classify(&e))? {
        if body.len() + chunk.len() > limit {
            return Err(FetchError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// [`body_limited`] with [`MAX_BODY`], as text. Not valid UTF-8 is a decode
/// failure at the start, like any body that is not what the API describes.
pub async fn text(response: Response) -> Result<String, FetchError> {
    let bytes = body_limited(response, MAX_BODY).await?;
    String::from_utf8(bytes).map_err(|_| FetchError::Decode { line: 0, column: 0 })
}

/// Status check and JSON decoding in one step. The body is read as bytes
/// first so that a decode failure is reported by position only.
pub async fn json<T: DeserializeOwned>(response: Response) -> Result<T, FetchError> {
    let status = response.status();
    if !status.is_success() {
        return Err(FetchError::Status(status.as_u16()));
    }
    let bytes = body_limited(response, MAX_BODY).await?;
    serde_json::from_slice(&bytes).map_err(|e| FetchError::Decode {
        line: e.line(),
        column: e.column(),
    })
}

pub async fn get_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, FetchError> {
    json(send(request).await?).await
}

/// Parses an instance's base URL once, at start-up. The error does not
/// repeat the URL: it may carry a key in its query.
pub fn base_url(raw: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw).map_err(|e| anyhow::anyhow!("invalid url: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("url must be http or https");
    }
    if url.cannot_be_a_base() {
        anyhow::bail!("url cannot be a base");
    }
    Ok(url)
}

/// `base` + `path` + `query`, keeping any URL base the service sits under
/// (`http://host:7878/radarr` + `/api/v3/queue`).
pub fn endpoint(base: &Url, path: &str, query: &[(&str, &str)]) -> Url {
    let mut url = base.clone();
    let joined = format!("{}{}", base.path().trim_end_matches('/'), path);
    url.set_path(&joined);
    url.set_query(None);
    url.set_fragment(None);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    url
}

/// A cookie jar for exactly one service. reqwest has one behind a feature,
/// but the two services that need cookies (qBittorrent, pyLoad) only need
/// "send back what you were given": one host, one path, no expiry logic
/// beyond "the server deleted it".
#[derive(Debug, Default)]
pub struct CookieJar(Mutex<BTreeMap<String, String>>);

impl CookieJar {
    pub fn store(&self, headers: &HeaderMap) {
        let mut jar = self.0.lock().unwrap_or_else(|p| p.into_inner());
        for value in headers.get_all(SET_COOKIE) {
            let Ok(value) = value.to_str() else { continue };
            let mut attributes = value.split(';');
            let Some((name, val)) = attributes.next().and_then(|p| p.split_once('=')) else {
                continue;
            };
            let deleted = val.trim().is_empty()
                || attributes.any(|a| a.trim().eq_ignore_ascii_case("max-age=0"));
            if deleted {
                jar.remove(name.trim());
            } else {
                jar.insert(name.trim().to_string(), val.trim().to_string());
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).is_empty()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// Adds the `Cookie` header when there is anything to send.
    pub fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        let jar = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if jar.is_empty() {
            return request;
        }
        let header = jar
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        request.header(COOKIE, header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn endpoint_keeps_the_url_base() {
        let base = base_url("http://10.0.2.5:7878/radarr/").unwrap();
        let url = endpoint(&base, "/api/v3/queue", &[("pageSize", "1000")]);
        assert_eq!(
            url.as_str(),
            "http://10.0.2.5:7878/radarr/api/v3/queue?pageSize=1000"
        );
        let bare = base_url("http://h:1").unwrap();
        assert_eq!(endpoint(&bare, "/api", &[]).as_str(), "http://h:1/api");
    }

    #[test]
    fn errors_print_a_class_and_nothing_else() {
        assert_eq!(FetchError::Status(401).to_string(), "status 401");
        assert_eq!(FetchError::Timeout.to_string(), "timeout");
    }

    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn answering(template: ResponseTemplate) -> (MockServer, Response) {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(template)
            .mount(&server)
            .await;
        let response = send(Client::new().get(server.uri())).await.unwrap();
        (server, response)
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_refused_and_one_at_the_limit_is_not() {
        let (_s, r) = answering(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 101])).await;
        assert_eq!(body_limited(r, 100).await, Err(FetchError::TooLarge));
        let (_s, r) = answering(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 100])).await;
        assert_eq!(body_limited(r, 100).await.map(|b| b.len()), Ok(100));
    }

    /// The limit holds without a Content-Length to believe: a chunked body
    /// that never ends is counted as it arrives and dropped at the limit,
    /// not read to the end. Written by hand, because wiremock always sends
    /// the length.
    #[tokio::test]
    async fn an_endless_chunked_body_is_cut_off_at_the_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await;
            let chunk = format!("400\r\n{}\r\n", "x".repeat(1024));
            // Ends only when the client hangs up.
            while socket.write_all(chunk.as_bytes()).await.is_ok() {}
        });
        let r = send(Client::new().get(format!("http://{addr}/")))
            .await
            .unwrap();
        assert_eq!(r.content_length(), None);
        assert_eq!(body_limited(r, 64 * 1024).await, Err(FetchError::TooLarge));
    }

    #[test]
    fn the_jar_keeps_updates_and_forgets_deleted_cookies() {
        let jar = CookieJar::default();
        let mut h = HeaderMap::new();
        h.append(
            SET_COOKIE,
            HeaderValue::from_static("a=1; Path=/; HttpOnly"),
        );
        h.append(SET_COOKIE, HeaderValue::from_static("b=2"));
        jar.store(&h);
        let mut h = HeaderMap::new();
        h.append(SET_COOKIE, HeaderValue::from_static("a=3"));
        h.append(SET_COOKIE, HeaderValue::from_static("b=; Max-Age=0"));
        jar.store(&h);
        let request = jar.apply(Client::new().get("http://h/")).build().unwrap();
        assert_eq!(request.headers()[COOKIE], "a=3");
    }
}
