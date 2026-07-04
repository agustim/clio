//! Bot de Telegram (long polling, sense dependències extra: només reqwest).
//!
//! Comportament: el bot només accepta links d'usuaris registrats (amb
//! `telegram_id` informat). Si l'emissor no és cap usuari conegut, calla. Si ho
//! és i el missatge conté alguna URL, l'encua i respon "Processant url.".

use crate::service::AppState;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

/// Emissor d'avisos d'admin a un chat fix de Telegram. Es comparteix dins
/// d'AppState perquè qualsevol part (cua, pipeline) pugui notificar.
pub struct Notifier {
    http: reqwest::Client,
    base: String,
    chat_id: i64,
}

impl Notifier {
    /// Construeix el notifier si hi ha token i chat_id configurats.
    pub fn build(token: &Option<String>, chat_id: Option<i64>) -> Option<Self> {
        let token = token.as_deref().filter(|t| !t.is_empty())?;
        let chat_id = chat_id?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .ok()?;
        Some(Self { http, base: format!("https://api.telegram.org/bot{token}"), chat_id })
    }

    /// Envia un avís (best-effort: els errors només es registren).
    pub async fn send(&self, text: &str) {
        send_message(&self.http, &self.base, self.chat_id, text, None).await;
    }

    /// Avís d'error amb botons d'acció (esborrar / reintentar el link).
    pub async fn send_error(&self, text: &str, link_id: Uuid) {
        let markup = json!({
            "inline_keyboard": [[
                { "text": "🗑 Esborra", "callback_data": format!("del:{link_id}") },
                { "text": "🔁 Reintenta", "callback_data": format!("retry:{link_id}") },
            ]]
        });
        send_message(&self.http, &self.base, self.chat_id, text, Some(markup)).await;
    }
}

pub async fn run(state: AppState) {
    let token = match &state.cfg.telegram_bot_token {
        Some(t) if !t.is_empty() => t.clone(),
        _ => {
            tracing::info!("Telegram desactivat (TELEGRAM_BOT_TOKEN buit)");
            return;
        }
    };
    // Client propi: el long poll manté la connexió oberta ~25s (més que el
    // timeout de 10s del client compartit).
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(40))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "telegram: no s'ha pogut crear el client");
            return;
        }
    };
    let base = format!("https://api.telegram.org/bot{token}");
    tracing::info!("Telegram bot actiu (long polling)");

    // Descarta el backlog: agafa l'últim update sense processar-lo, per no
    // respondre missatges acumulats mentre el bot estava aturat.
    let mut offset: i64 = drain_backlog(&http, &base).await.unwrap_or_default();
    state.notify("✅ Clio actiu.").await;
    loop {
        let resp = http
            .get(format!("{base}/getUpdates"))
            .query(&[("offset", offset.to_string()), ("timeout", "25".to_string())])
            .send()
            .await;
        let body: Value = match resp {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "telegram: getUpdates JSON invàlid");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "telegram: getUpdates ha fallat");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        for up in body["result"].as_array().cloned().unwrap_or_default() {
            if let Some(id) = up["update_id"].as_i64() {
                offset = offset.max(id + 1);
            }
            if up.get("callback_query").is_some() {
                handle_callback(&state, &http, &base, &up["callback_query"]).await;
            } else {
                handle_update(&state, &http, &base, &up).await;
            }
        }
    }
}

/// Demana l'últim update (offset=-1) i retorna el següent offset a usar, de
/// manera que els updates anteriors quedin confirmats i no es reprocessin.
async fn drain_backlog(http: &reqwest::Client, base: &str) -> Option<i64> {
    let body: Value = http
        .get(format!("{base}/getUpdates"))
        .query(&[("offset", "-1"), ("timeout", "0")])
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let last = body["result"].as_array()?.last()?;
    last["update_id"].as_i64().map(|id| id + 1)
}

async fn handle_update(state: &AppState, http: &reqwest::Client, base: &str, up: &Value) {
    let msg = &up["message"];
    let text = msg["text"].as_str().unwrap_or("");
    let (Some(chat_id), Some(from_id)) = (msg["chat"]["id"].as_i64(), msg["from"]["id"].as_i64())
    else {
        return;
    };

    // Només usuaris registrats (amb telegram_id). Desconeguts: silenci.
    let user = match state.db.user_by_telegram_id(&from_id.to_string()).await {
        Ok(Some(u)) => u,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, "telegram: cerca d'usuari ha fallat");
            return;
        }
    };

    let urls = extract_urls(text);
    if urls.is_empty() {
        return;
    }
    for raw in &urls {
        match state.report_link(&user, raw).await {
            Ok(o) => {
                if o.needs_processing {
                    state.enqueue(o.link_id);
                }
            }
            Err(e) => tracing::warn!(error = %e, url = %raw, "telegram: report_link ha fallat"),
        }
    }
    send_message(http, base, chat_id, "Processant url.", None).await;
    tracing::info!(user = %user.username, n = urls.len(), "telegram: links encuats");
}

/// Gestiona els botons dels avisos d'error (esborra / reintenta). Només accepta
/// callbacks provinents del chat d'admin configurat.
async fn handle_callback(state: &AppState, http: &reqwest::Client, base: &str, cb: &Value) {
    let cb_id = cb["id"].as_str().unwrap_or("");
    let chat_id = cb["message"]["chat"]["id"].as_i64();
    let message_id = cb["message"]["message_id"].as_i64();

    if chat_id != state.cfg.admin_chat_id {
        answer_callback(http, base, cb_id, "No autoritzat.").await;
        return;
    }

    let Some((action, id_str)) = cb["data"].as_str().and_then(|d| d.split_once(':')) else {
        return;
    };
    let Ok(link_id) = Uuid::parse_str(id_str) else {
        return;
    };

    let note = match action {
        "del" => match state.db.delete_link(link_id).await {
            Ok(true) => "🗑 Link esborrat.",
            Ok(false) => "El link ja no existeix.",
            Err(e) => {
                tracing::warn!(error = %e, %link_id, "telegram: esborrar link ha fallat");
                "Error esborrant el link."
            }
        },
        "retry" => match state.retry_link(link_id).await {
            Ok(()) => "🔁 Link reencuat.",
            Err(e) => {
                tracing::warn!(error = %e, %link_id, "telegram: reintent ha fallat");
                "Error reencuant el link."
            }
        },
        _ => "Acció desconeguda.",
    };

    answer_callback(http, base, cb_id, note).await;
    // Treu els botons i deixa constància de l'acció al missatge original.
    if let (Some(cid), Some(mid)) = (chat_id, message_id) {
        let orig = cb["message"]["text"].as_str().unwrap_or("");
        edit_message(http, base, cid, mid, &format!("{orig}\n\n{note}")).await;
    }
}

async fn answer_callback(http: &reqwest::Client, base: &str, cb_id: &str, text: &str) {
    let r = http
        .post(format!("{base}/answerCallbackQuery"))
        .json(&json!({ "callback_query_id": cb_id, "text": text }))
        .send()
        .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, "telegram: answerCallbackQuery ha fallat");
    }
}

async fn edit_message(http: &reqwest::Client, base: &str, chat_id: i64, message_id: i64, text: &str) {
    // Sense reply_markup: editMessageText elimina el teclat inline.
    let r = http
        .post(format!("{base}/editMessageText"))
        .json(&json!({ "chat_id": chat_id, "message_id": message_id, "text": text }))
        .send()
        .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, "telegram: editMessageText ha fallat");
    }
}

/// Extreu les URLs http(s) del text (tokens separats per espais).
fn extract_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|t| t.starts_with("http://") || t.starts_with("https://"))
        .map(|s| s.trim_end_matches(['.', ',', ')', ']', '}', '"', '\'']).to_string())
        .collect()
}

async fn send_message(
    http: &reqwest::Client,
    base: &str,
    chat_id: i64,
    text: &str,
    reply_markup: Option<Value>,
) {
    let mut payload = json!({ "chat_id": chat_id, "text": text });
    if let Some(markup) = reply_markup {
        payload["reply_markup"] = markup;
    }
    let r = http
        .post(format!("{base}/sendMessage"))
        .json(&payload)
        .send()
        .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, "telegram: sendMessage ha fallat");
    }
}
