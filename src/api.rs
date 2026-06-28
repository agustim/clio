use crate::error::{AppError, Result};
use crate::models::{LinkType, Sentiment, User};
use crate::service::AppState;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/links", post(create_link).get(list_links))
        .route("/api/v1/links/:id", get(get_link))
        .route("/api/v1/stats", get(stats))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Autenticació via `Authorization: Bearer <token>` o camp `token` al body.
async fn auth(state: &AppState, headers: &HeaderMap, body_token: Option<&str>) -> Result<User> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or_else(|| body_token.map(|s| s.to_string()))
        .ok_or(AppError::Unauthorized)?;

    state.db.user_by_token(&token).await?.ok_or(AppError::Unauthorized)
}

#[derive(Deserialize)]
struct CreateLinkReq {
    url: String,
    token: Option<String>,
}

async fn create_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateLinkReq>,
) -> Result<Json<Value>> {
    let user = auth(&state, &headers, body.token.as_deref()).await?;
    let outcome = state.report_link(&user, &body.url).await?;
    if outcome.needs_processing {
        state.spawn_pipeline(outcome.link_id);
    }
    Ok(Json(json!({
        "link_id": outcome.link_id,
        "is_new": outcome.is_new,
        "added_reporter": outcome.added_reporter,
        "status": "queued",
    })))
}

#[derive(Deserialize)]
struct ListParams {
    tag: Option<String>,
    sentiment: Option<String>,
    link_type: Option<String>,
    limit: Option<i64>,
}

async fn list_links(
    State(state): State<AppState>,
    Query(p): Query<ListParams>,
) -> Result<Json<Value>> {
    let sentiment = p.sentiment.as_deref().map(Sentiment::from_db);
    let link_type = p.link_type.as_deref().map(LinkType::from_db);
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    let links = state
        .db
        .list_links(p.tag.as_deref(), sentiment, link_type, limit)
        .await?;
    Ok(Json(json!({ "count": links.len(), "links": links })))
}

async fn get_link(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let uuid = Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("bad id".into()))?;
    let link = state.db.link_by_id(uuid).await?.ok_or(AppError::NotFound)?;
    Ok(Json(json!(link)))
}

async fn stats(State(state): State<AppState>) -> Result<Json<Value>> {
    let s = state.db.stats().await?;
    Ok(Json(json!(s)))
}
