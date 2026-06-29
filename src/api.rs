use crate::error::{AppError, Result};
use crate::models::{DeepStatus, LinkStatus, LinkType, Sentiment, User, UserRole};
use crate::service::AppState;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, patch, post};
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
        .route("/api/v1/ping", get(ping))
        .route("/api/v1/me", get(me))
        .route("/api/v1/links", post(create_link).get(list_links))
        .route("/api/v1/links/:id", get(get_link).delete(delete_link))
        .route("/api/v1/links/:id/reprocess", post(reprocess_link))
        .route("/api/v1/users", get(users_list).post(users_create))
        .route("/api/v1/users/:id", patch(users_update).delete(users_delete))
        .route("/api/v1/users/:id/token", post(users_regen_token))
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

/// Com `auth` però exigeix rol admin.
async fn admin(state: &AppState, headers: &HeaderMap) -> Result<User> {
    let user = auth(state, headers, None).await?;
    if user.role != UserRole::Admin {
        return Err(AppError::Forbidden);
    }
    Ok(user)
}

/// Sonda perquè el client sàpiga que parla amb un servei viu (mode `serve`).
/// La web estàtica pura no respon aquí i amaga les accions.
async fn ping() -> Json<Value> {
    Json(json!({ "service": "clio", "serve": true }))
}

/// Identitat de l'usuari del token (perquè la web sàpiga el rol).
async fn me(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>> {
    let u = auth(&state, &headers, None).await?;
    Ok(Json(json!({ "id": u.id, "username": u.username, "role": u.role })))
}

// ---- Gestió d'usuaris (només admin) ----

async fn users_list(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>> {
    admin(&state, &headers).await?;
    let users = state.db.list_users().await?; // api_token no es serialitza (skip)
    Ok(Json(json!({ "count": users.len(), "users": users })))
}

/// Comprova que cap ALTRE usuari ja tingui aquest telegram_id.
async fn telegram_id_free(state: &AppState, tid: &str, except: Option<Uuid>) -> Result<()> {
    if tid.is_empty() {
        return Ok(());
    }
    if let Some(other) = state.db.user_by_telegram_id(tid).await? {
        if Some(other.id) != except {
            return Err(AppError::BadRequest(
                "aquest telegram_id ja està assignat a un altre usuari".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct CreateUserReq {
    username: String,
    #[serde(default)]
    admin: bool,
    telegram_id: Option<String>,
}

async fn users_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateUserReq>,
) -> Result<Json<Value>> {
    admin(&state, &headers).await?;
    let username = body.username.trim();
    if username.is_empty() {
        return Err(AppError::BadRequest("username buit".into()));
    }
    if state.db.user_by_username(username).await?.is_some() {
        return Err(AppError::BadRequest("ja existeix un usuari amb aquest nom".into()));
    }
    let tid = body.telegram_id.as_deref().map(str::trim).unwrap_or("");
    telegram_id_free(&state, tid, None).await?;

    let role = if body.admin { UserRole::Admin } else { UserRole::User };
    let mut u = state.db.create_user(username, role).await?;
    if !tid.is_empty() {
        u = state
            .db
            .update_user(u.id, None, None, Some(tid))
            .await?
            .ok_or(AppError::NotFound)?;
    }
    // El token només es mostra aquí (a la creació) i en regenerar-lo.
    Ok(Json(json!({
        "id": u.id, "username": u.username, "role": u.role,
        "telegram_id": u.telegram_id, "api_token": u.api_token,
    })))
}

#[derive(Deserialize)]
struct UpdateUserReq {
    username: Option<String>,
    admin: Option<bool>,
    telegram_id: Option<String>,
}

async fn users_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UpdateUserReq>,
) -> Result<Json<Value>> {
    let me = admin(&state, &headers).await?;
    let uuid = Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("bad id".into()))?;
    // Evita que un admin es tregui a si mateix els privilegis i quedi sense admins.
    if uuid == me.id && body.admin == Some(false) {
        return Err(AppError::BadRequest("no et pots treure el rol admin a tu mateix".into()));
    }
    let role = body.admin.map(|a| if a { UserRole::Admin } else { UserRole::User });
    let username = body.username.as_deref().map(str::trim).filter(|s| !s.is_empty());
    // telegram_id: Some("") esborra; Some(x) assigna; None no toca.
    let telegram_id = body.telegram_id.as_deref().map(str::trim);
    if let Some(tid) = telegram_id {
        telegram_id_free(&state, tid, Some(uuid)).await?;
    }
    let u = state
        .db
        .update_user(uuid, username, role, telegram_id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!({
        "id": u.id, "username": u.username, "role": u.role, "telegram_id": u.telegram_id,
    })))
}

async fn users_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let me = admin(&state, &headers).await?;
    let uuid = Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("bad id".into()))?;
    if uuid == me.id {
        return Err(AppError::BadRequest("no et pots esborrar a tu mateix".into()));
    }
    let deleted = state.db.delete_user(uuid).await?;
    Ok(Json(json!({ "id": uuid, "deleted": deleted })))
}

async fn users_regen_token(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    admin(&state, &headers).await?;
    let uuid = Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("bad id".into()))?;
    let token = state.db.regenerate_token(uuid).await?.ok_or(AppError::NotFound)?;
    Ok(Json(json!({ "id": uuid, "api_token": token })))
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
