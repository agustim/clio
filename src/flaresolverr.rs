//! Fallback per a pàgines protegides per murs anti-bot (Cloudflare, DDoS-Guard,
//! etc.) que responen 403/429/503 a un GET normal. FlareSolverr és un servei
//! extern que resol el challenge amb un navegador headless (Chromium) i retorna
//! l'HTML final ja renderitzat.
//!
//! Protocol: POST {base}/v1 amb {"cmd":"request.get","url":...,"maxTimeout":ms}.
//! Docs: https://github.com/FlareSolverr/FlareSolverr

use crate::error::{AppError, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// Resol `url` via FlareSolverr i retorna l'HTML. `base` és la URL del servei
/// (p.ex. http://flaresolverr:8191), `timeout_secs` el temps màxim de resolució.
pub async fn fetch(base: &str, url: &str, timeout_secs: u64, max_bytes: usize) -> Result<String> {
    let endpoint = format!("{}/v1", base.trim_end_matches('/'));
    // El client compartit té un timeout curt (10s); un solve headless triga més,
    // així que en construïm un de dedicat amb marge sobre el maxTimeout.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs + 15))
        .build()?;

    let body = json!({
        "cmd": "request.get",
        "url": url,
        "maxTimeout": timeout_secs * 1000,
    });

    let resp = client
        .post(&endpoint)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    let v: Value = resp.json().await?;

    if v.get("status").and_then(Value::as_str) != Some("ok") {
        let msg = v.get("message").and_then(Value::as_str).unwrap_or("unknown");
        return Err(AppError::Pipeline(format!("flaresolverr: {msg}")));
    }

    let html = v
        .get("solution")
        .and_then(|s| s.get("response"))
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Pipeline("flaresolverr: resposta sense solution.response".into()))?;

    if html.len() > max_bytes {
        return Err(AppError::Pipeline(format!("body too large: {} bytes", html.len())));
    }
    Ok(html.to_string())
}
