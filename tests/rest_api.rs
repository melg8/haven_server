//! End-to-end REST flow tests: the router is driven in-process via
//! `tower::ServiceExt::oneshot` — no sockets, no spawned tasks.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use haven_server::api;
use haven_server::config::Config;
use haven_server::match_manager::MatchManager;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

fn app() -> axum::Router {
    let mgr = MatchManager::new(Config::default());
    api::build_router(mgr)
}

async fn req(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (StatusCode, Value) {
    let (status, text) = req_raw(app, method, uri, body).await;
    let value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{method} {uri}: body is not JSON ({e}): {text:.120}"));
    (status, value)
}

/// Same as `req` but returns the raw body (for HTML/text responses).
async fn req_raw(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let res = app
        .oneshot(builder.body(Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn healthz_and_index() {
    let app = app();
    let (status, body) = req(app.clone(), "GET", "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], Value::Bool(true));

    // The embedded web client is HTML, not JSON.
    let (status, html) = req_raw(app, "GET", "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("haven_server"),
        "index must serve the web client"
    );
}

#[tokio::test]
async fn create_join_full_flow() {
    let app = app();

    // --- create -----------------------------------------------------------
    let overrides = r#"{"simulation": {"tick_rate": 10, "unit_hp": 3}}"#;
    let (status, body) = req(app.clone(), "POST", "/match", Some(overrides.into())).await;
    assert_eq!(status, StatusCode::OK);
    let match_id = body["match_id"].as_str().expect("match_id").to_string();
    assert_eq!(match_id.len(), 12);
    // Effective config echoes the override and fills defaults.
    assert_eq!(body["config"]["simulation"]["unit_hp"], 3);
    assert_eq!(body["config"]["simulation"]["tick_rate"], 10);
    assert_eq!(body["config"]["simulation"]["melee_damage"], 1);

    // --- info: lobby, nobody joined --------------------------------------
    let (status, info) = req(app.clone(), "GET", &format!("/match/{match_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["status"], "lobby");
    assert_eq!(info["players"]["white"], false);
    assert_eq!(info["players"]["black"], false);
    assert_eq!(info["units_alive"]["white"], 8);

    // --- join: white then black ------------------------------------------
    let (status, j1) = req(
        app.clone(),
        "POST",
        &format!("/match/{match_id}/join"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j1["owner"], "white");
    let token1 = j1["player_token"].as_str().unwrap().to_string();

    let (status, j2) = req(
        app.clone(),
        "POST",
        &format!("/match/{match_id}/join"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(j2["owner"], "black");
    let token2 = j2["player_token"].as_str().unwrap().to_string();
    assert_ne!(token1, token2);

    // Third join is rejected: match is full.
    let (status, err) = req(
        app.clone(),
        "POST",
        &format!("/match/{match_id}/join"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err["error"], "match is full");

    // Match auto-started when the second side joined.
    let (_, info) = req(app.clone(), "GET", &format!("/match/{match_id}"), None).await;
    assert_eq!(info["status"], "active");
    assert_eq!(info["players"]["white"], true);
    assert_eq!(info["players"]["black"], true);

    // --- rejoin with an existing token returns the same side --------------
    let (status, rejoin) = req(
        app.clone(),
        "POST",
        &format!("/match/{match_id}/join"),
        Some(format!(r#"{{"token":"{token1}"}}"#)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rejoin["owner"], "white");
    assert_eq!(rejoin["player_token"], token1);
}

#[tokio::test]
async fn bad_requests() {
    let app = app();

    // Unknown match info -> 404.
    let (status, err) = req(app.clone(), "GET", "/match/doesnotexist", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["error"], "match not found");

    // Unknown match join -> 404.
    let (status, _) = req(app.clone(), "POST", "/match/doesnotexist/join", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Invalid config override -> 400.
    let (status, err) = req(
        app.clone(),
        "POST",
        "/match",
        Some(r#"{"simulation": {"tick_rate": 100000}}"#.into()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["error"].as_str().unwrap().contains("tick_rate"));

    // Malformed JSON -> 400 (rejection body is text, hence req_raw).
    let (status, _) = req_raw(app, "POST", "/match", Some("{oops".into())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn max_matches_guard() {
    // A manager with max_matches = 1 rejects the second creation.
    let mut cfg = Config::default();
    cfg.server.max_matches = 1;
    let app = api::build_router(MatchManager::new(cfg));

    let (status, _) = req(app.clone(), "POST", "/match", None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, err) = req(app, "POST", "/match", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(err["error"].as_str().unwrap().contains("max_matches"));
}
