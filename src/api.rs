use crate::error::{AppError, Result};
use crate::models::{DeepStatus, LinkStatus, LinkType, Sentiment, User, UserRole};
use crate::service::AppState;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

pub fn router(state: AppState) -> Router {
    // Web estàtica a l'arrel (fora de /api): serveix cfg.public_dir, amb
    // index.html per a directoris.
    let static_files =
        ServeDir::new(state.cfg.public_dir.clone()).append_index_html_on_directories(true);

    Router::new()
        .route("/api/v1/links", post(create_link).get(list_links))
        .route("/api/v1/links/:id", get(get_link).delete(delete_link))
        .route("/api/v1/links/:id/reprocess", post(reprocess_link))
        .route("/api/v1/stats", get(stats))
        // Tot el que no és /api cau a la web estàtica.
        .fallback_service(static_files)
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
    /// Un sol enllaç.
    url: Option<String>,
    /// O un lot d'enllaços.
    #[serde(default)]
    urls: Vec<String>,
    token: Option<String>,
}

async fn create_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateLinkReq>,
) -> Result<Json<Value>> {
    let user = auth(&state, &headers, body.token.as_deref()).await?;

    // Accepta `url` (un) o `urls` (lot). El lot té prioritat si ve ple.
    let urls: Vec<String> = if !body.urls.is_empty() {
        body.urls
    } else {
        body.url.into_iter().collect()
    };
    if urls.is_empty() {
        return Err(AppError::BadRequest("cal 'url' o 'urls'".into()));
    }

    let mut results = Vec::with_capacity(urls.len());
    for raw in urls {
        match state.report_link(&user, &raw).await {
            Ok(outcome) => {
                if outcome.needs_processing {
                    state.enqueue(outcome.link_id);
                }
                results.push(json!({
                    "url": raw,
                    "link_id": outcome.link_id,
                    "is_new": outcome.is_new,
                    "added_reporter": outcome.added_reporter,
                    "status": "queued",
                }));
            }
            Err(e) => results.push(json!({ "url": raw, "error": e.to_string() })),
        }
    }

    // Compat: si era un sol enllaç, retorna l'objecte directament.
    if results.len() == 1 {
        Ok(Json(results.into_iter().next().unwrap()))
    } else {
        Ok(Json(json!({ "count": results.len(), "results": results })))
    }
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

/// Reforça un link: el reencua perquè es torni a analitzar de zero (útil quan
/// l'LLM o el fetch han fallat). Qualsevol usuari autenticat pot fer-ho.
async fn reprocess_link(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let _user = auth(&state, &headers, None).await?;
    let uuid = Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("bad id".into()))?;
    state.db.link_by_id(uuid).await?.ok_or(AppError::NotFound)?;

    // Reinicia estat perquè el pipeline (shallow -> deep) torni a córrer.
    state.db.set_link_status(uuid, LinkStatus::Pending).await?;
    state.db.set_deep_status(uuid, DeepStatus::None).await?;
    state.enqueue(uuid);

    Ok(Json(json!({ "link_id": uuid, "status": "queued" })))
}

/// Dona de baixa un link. Permès a admins o a qualsevol dels seus reporters.
async fn delete_link(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let user = auth(&state, &headers, None).await?;
    let uuid = Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("bad id".into()))?;
    let link = state.db.link_by_id(uuid).await?.ok_or(AppError::NotFound)?;

    let allowed = user.role == UserRole::Admin || link.co_reporters.contains(&user.id);
    if !allowed {
        return Err(AppError::Forbidden);
    }

    let deleted = state.db.delete_link(uuid).await?;
    if deleted {
        state.web_dirty.notify_one();
    }
    Ok(Json(json!({ "link_id": uuid, "deleted": deleted })))
}

async fn stats(State(state): State<AppState>) -> Result<Json<Value>> {
    let s = state.db.stats().await?;
    Ok(Json(json!(s)))
}
