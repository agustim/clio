//! Bot de Telegram (long polling, sense dependències extra: només reqwest).
//!
//! Comportament: el bot només accepta links d'usuaris registrats (amb
//! `telegram_id` informat). Si l'emissor no és cap usuari conegut, calla. Si ho
//! és i el missatge conté alguna URL, l'encua i respon "Processant url.".

use crate::service::AppState;
use serde_json::{json, Value};
use std::time::Duration;

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
        send_message(&self.http, &self.base, self.chat_id, text).await;
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
            handle_update(&state, &http, &base, &up).await;
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
    send_message(http, base, chat_id, "Processant url.").await;
    tracing::info!(user = %user.username, n = urls.len(), "telegram: links encuats");
}

/// Extreu les URLs http(s) del text (tokens separats per espais).
fn extract_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|t| t.starts_with("http://") || t.starts_with("https://"))
        .map(|s| s.trim_end_matches(['.', ',', ')', ']', '}', '"', '\'']).to_string())
        .collect()
}

async fn send_message(http: &reqwest::Client, base: &str, chat_id: i64, text: &str) {
    let r = http
        .post(format!("{base}/sendMessage"))
        .json(&json!({ "chat_id": chat_id, "text": text }))
        .send()
        .await;
    if let Err(e) = r {
        tracing::warn!(error = %e, "telegram: sendMessage ha fallat");
    }
}
