//! `GET /snapshot` only with the bearer token, `GET /healthz` for anyone,
//! nothing else.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use jiff::Timestamp;
use queuewatch::collect::{Latest, router};
use queuewatch::snapshot::{IndexerSummary, SCHEMA_VERSION, Snapshot};
use tower::ServiceExt;

const TOKEN: &str = "s3cr3t-token-of-some-length";

async fn call(app: axum::Router, uri: &str, auth: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut request = Request::get(uri);
    if let Some(a) = auth {
        request = request.header("authorization", a);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

fn empty_snapshot() -> Snapshot {
    Snapshot {
        version: SCHEMA_VERSION,
        taken_at: Timestamp::now(),
        sources: vec![],
        queue: vec![],
        indexers: IndexerSummary::default(),
        calendar: vec![],
        recent: vec![],
        requests: vec![],
    }
}

#[tokio::test]
async fn the_snapshot_needs_the_token_and_a_finished_round() {
    let latest = Latest::default();
    let app = router(latest.clone(), TOKEN.into());
    let bearer = format!("Bearer {TOKEN}");

    assert_eq!(
        call(app.clone(), "/snapshot", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    // The token is checked before anything else is said, even "not yet".
    assert_eq!(
        call(app.clone(), "/snapshot", Some("Bearer wrong")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(app.clone(), "/snapshot", Some(TOKEN)).await.0,
        StatusCode::UNAUTHORIZED,
        "the token without the Bearer scheme"
    );
    assert_eq!(
        call(app.clone(), "/snapshot", Some(&bearer)).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );

    let snapshot = empty_snapshot();
    latest.set(snapshot.clone());
    let (status, body) = call(app.clone(), "/snapshot", Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK);
    let got: Snapshot = serde_json::from_slice(&body).unwrap();
    assert_eq!(got, snapshot);
    assert_eq!(
        call(app, "/snapshot", Some("Bearer wrong")).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn healthz_answers_without_a_token_and_nothing_else_exists() {
    let app = router(Latest::default(), TOKEN.into());
    let (status, body) = call(app.clone(), "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
    let bearer = format!("Bearer {TOKEN}");
    assert_eq!(call(app, "/", Some(&bearer)).await.0, StatusCode::NOT_FOUND);
}
