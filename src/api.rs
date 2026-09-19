//! REST API handlers.
//!
//! - `POST /match`         — create a match
//! - `GET  /match/{id}`    — public match info
//! - `POST /match/{id}/join` — join as a player (auto-starts the match)
//! - `GET  /ws/{match_id}?token=…` — WebSocket (see `network.rs`)
//! - `GET  /`              — embedded browser client
//! - `GET  /healthz`       — liveness

use crate::match_manager::{CreateMatchRequest, JoinRequest, MatchManager};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

pub type AppShared = Arc<MatchManager>;

/// Build the full application router (REST + WS + web client).
pub fn build_router(manager: AppShared) -> axum::Router {
    axum::Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/match", post(create_match).get(list_matches))
        .route("/match/{match_id}", get(match_info))
        .route("/match/{match_id}/join", post(join_match))
        .route("/ws/{match_id}", get(ws_route))
        .with_state(manager)
}

pub async fn create_match(
    State(mgr): State<AppShared>,
    body: Option<Json<CreateMatchRequest>>,
) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    match mgr.create(req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) if e.contains("max_matches") => error_json(StatusCode::SERVICE_UNAVAILABLE, &e),
        Err(e) => error_json(StatusCode::BAD_REQUEST, &e),
    }
}

pub async fn match_info(State(mgr): State<AppShared>, Path(match_id): Path<String>) -> Response {
    match mgr.info(&match_id).await {
        Some(info) => (StatusCode::OK, Json(info)).into_response(),
        None => error_json(StatusCode::NOT_FOUND, "match not found"),
    }
}

pub async fn join_match(
    State(mgr): State<AppShared>,
    Path(match_id): Path<String>,
    body: Option<Json<JoinRequest>>,
) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    match mgr.join(&match_id, req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) if e == "match not found" => error_json(StatusCode::NOT_FOUND, &e),
        Err(e) if e == "match is full" => error_json(StatusCode::CONFLICT, &e),
        Err(e) => error_json(StatusCode::BAD_REQUEST, &e),
    }
}

pub async fn list_matches(State(mgr): State<AppShared>) -> Response {
    // Cheap public summary: count only. Full enumeration is a debug feature.
    (
        StatusCode::OK,
        Json(json!({ "live_matches": mgr.live_count().await })),
    )
        .into_response()
}

pub async fn healthz() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

pub async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

/// WebSocket upgrade for `/ws/{match_id}?token=...`.
pub async fn ws_route(
    State(mgr): State<AppShared>,
    Path(match_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    upgrade: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    let Some(handle) = mgr.get(&match_id).await else {
        return error_json(StatusCode::NOT_FOUND, "match not found");
    };
    let role = match params.get("token").map(|t| t.as_str()) {
        Some(token) => handle.role_for_token(token).await,
        None => crate::network::Role::Spectator,
    };

    upgrade
        .on_upgrade(move |socket| crate::network::run_connection(socket, role, handle))
        .into_response()
}

fn error_json(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
