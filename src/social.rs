//! Extractors per a xarxes socials que renderitzen el contingut amb JS i, per
//! tant, no es poden llegir amb un simple GET (X/Twitter, Bluesky). Usem APIs
//! públiques de només lectura que retornen el text del post.

use crate::error::Result;
use crate::pipeline::Parsed;
use serde_json::Value;

/// Si la URL és d'una xarxa coneguda, n'extreu el contingut. Retorna `Ok(None)`
/// si no aplica o si l'API falla (perquè el caller faci el fetch normal).
pub async fn extract(http: &reqwest::Client, url: &str) -> Result<Option<Parsed>> {
    let u = url.to_lowercase();
    if u.contains("x.com/") || u.contains("twitter.com/") {
        return Ok(extract_tweet(http, url).await);
    }
    if u.contains("bsky.app/") {
        return Ok(extract_bsky(http, url).await);
    }
    Ok(None)
}

/// Camí després de l'esquema i el host: `host/aa/bb` -> `aa/bb` (sense query).
fn path_of(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = after_scheme.split_once('/').map(|(_, p)| p)?;
    Some(path.split(['?', '#']).next().unwrap_or(path).trim_end_matches('/'))
}

/// X/Twitter via fxtwitter (JSON públic amb el text del tweet).
async fn extract_tweet(http: &reqwest::Client, url: &str) -> Option<Parsed> {
    let path = path_of(url)?; // p.ex. "_guillecasaus/status/123"
    if !path.contains("/status/") {
        return None;
    }
    let api = format!("https://api.fxtwitter.com/{path}");
    let resp = http.get(&api).send().await.ok()?.error_for_status().ok()?;
    let v: Value = resp.json().await.ok()?;
    let t = &v["tweet"];
    let text = t["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return None;
    }
    let name = t["author"]["name"].as_str().unwrap_or("");
    let screen = t["author"]["screen_name"].as_str().unwrap_or("");
    let title = social_title(name, screen);
    tracing::info!(%url, "social: tweet extret via fxtwitter");
    Some(Parsed { title, text, og_type: None })
}

/// Bluesky via l'API pública AT Protocol (getPostThread).
async fn extract_bsky(http: &reqwest::Client, url: &str) -> Option<Parsed> {
    let path = path_of(url)?; // "profile/<handle>/post/<rkey>"
    let parts: Vec<&str> = path.split('/').collect();
    let pi = parts.iter().position(|&p| p == "profile")?;
    let handle = parts.get(pi + 1)?;
    let post_i = parts.iter().position(|&p| p == "post")?;
    let rkey = parts.get(post_i + 1)?;
    let uri = format!("at://{handle}/app.bsky.feed.post/{rkey}");

    let resp = http
        .get("https://public.api.bsky.app/xrpc/app.bsky.feed.getPostThread")
        .query(&[("uri", &uri)])
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let v: Value = resp.json().await.ok()?;
    let post = &v["thread"]["post"];
    let text = post["record"]["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return None;
    }
    let name = post["author"]["displayName"].as_str().unwrap_or("");
    let title = social_title(name, handle);
    tracing::info!(%url, "social: post Bluesky extret");
    Some(Parsed { title, text, og_type: None })
}

/// Títol de reserva (l'LLM en generarà un de millor a partir del text).
fn social_title(name: &str, handle: &str) -> Option<String> {
    match (name.is_empty(), handle.is_empty()) {
        (false, false) => Some(format!("{name} (@{handle})")),
        (false, true) => Some(name.to_string()),
        (true, false) => Some(format!("@{handle}")),
        _ => None,
    }
}
