//! Extractors per a xarxes socials que renderitzen el contingut amb JS i, per
//! tant, no es poden llegir amb un simple GET (X/Twitter, Bluesky). Usem APIs
//! públiques de només lectura que retornen el text del post.
//!
//! Per a X/Twitter, a més del text, desem la fitxa pública del twitt (autor,
//! data, mètriques, media) perquè la web la mostri i la segona passada (deep)
//! en faci una anàlisi en català.

use crate::error::Result;
use crate::pipeline::Parsed;
use serde_json::Value;

/// Fitxa pública d'un twitt obtinguda de l'API de fxtwitter.
#[derive(Debug, Clone, Default)]
pub struct TweetInfo {
    pub text: String,
    pub author_name: String,
    pub author_screen: String,
    pub author_url: Option<String>,
    pub avatar_url: Option<String>,
    pub followers: u64,
    /// Data del twitt en ISO 8601 (UTC), si l'API en dóna el timestamp.
    pub created_at: Option<String>,
    pub created_timestamp: Option<i64>,
    pub likes: u64,
    pub retweets: u64,
    pub replies: u64,
    pub quotes: u64,
    pub bookmarks: u64,
    pub views: u64,
    pub lang: String,
    pub media: Vec<MediaItem>,
}

/// Un adjunt del twitt: foto, vídeo o GIF.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaItem {
    /// `photo` | `video` | `gif`.
    pub kind: String,
    /// URL directa del recurs (imatge o mp4).
    pub url: String,
    /// Miniatura (vídeos i GIFs en tenen; les fotos no).
    pub thumbnail_url: Option<String>,
}

impl MediaItem {
    fn from_json(v: &Value) -> Option<Self> {
        let kind = v["type"].as_str()?.trim().to_lowercase();
        if !matches!(kind.as_str(), "photo" | "video" | "gif") {
            return None;
        }
        let url = v["url"].as_str()?.trim().to_string();
        if !is_http(&url) {
            return None;
        }
        let thumb = v["thumbnail_url"].as_str().map(|s| s.trim().to_string()).filter(|s| is_http(s));
        Some(Self { kind, url, thumbnail_url: thumb })
    }
}

impl TweetInfo {
    /// Construeix la fitxa a partir del JSON de fxtwitter
    /// (`{"code":200,"tweet":{...}}`). `None` si no hi ha twitt (esborrat o 404).
    pub fn from_json(v: &Value) -> Option<Self> {
        // `t` ha de ser un Value (no un Map): l'indexació de Value retorna Null
        // per a claus absents, mentre que la de Map peta.
        let t = v.get("tweet")?;
        if !t.is_object() {
            return None;
        }
        let u = |x: &Value| x.as_u64().unwrap_or(0);
        let s = |x: &Value| x.as_str().unwrap_or("").trim().to_string();
        let ts = t.get("created_timestamp").and_then(Value::as_i64);
        let media = t["media"]["all"]
            .as_array()
            .map(|arr| arr.iter().filter_map(MediaItem::from_json).collect())
            .unwrap_or_default();
        Some(Self {
            text: s(&t["text"]),
            author_name: s(&t["author"]["name"]),
            author_screen: s(&t["author"]["screen_name"]),
            author_url: opt_str(&t["author"]["url"]),
            avatar_url: opt_str(&t["author"]["avatar_url"]),
            followers: u(&t["author"]["followers"]),
            created_at: ts
                .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            created_timestamp: ts,
            likes: u(&t["likes"]),
            retweets: u(&t["retweets"]),
            replies: u(&t["replies"]),
            quotes: u(&t["quotes"]),
            bookmarks: u(&t["bookmarks"]),
            views: u(&t["views"]),
            lang: s(&t["lang"]),
            media,
        })
    }

    /// Imatge d'acompanyament: la primera foto o, si no n'hi ha, la miniatura
    /// del primer vídeo/GIF. Només URLs http(s) absolutes.
    pub fn image(&self) -> Option<String> {
        self.media
            .iter()
            .find(|m| m.kind == "photo")
            .map(|m| m.url.clone())
            .or_else(|| self.media.iter().find_map(|m| m.thumbnail_url.clone()))
            .filter(|u| is_http(u))
    }
}

fn is_http(s: &str) -> bool {
    s.starts_with("https://") || s.starts_with("http://")
}

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Host en minúscules d'una URL (sense credencials ni port).
fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    host.rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Cert si la URL és de X/Twitter (x.com, twitter.com o un subdomini seu).
/// Match pel host, no per substring (`ex.com` no és `x.com`).
pub fn is_twitter_url(url: &str) -> bool {
    let host = host_of(url.trim());
    host == "x.com"
        || host == "twitter.com"
        || host.ends_with(".x.com")
        || host.ends_with(".twitter.com")
}

/// Camí canònic per a l'API de fxtwitter: `{usuari}/status/{id}`.
///
/// Accepta x.com/twitter.com (i subdominis), treu query/hash i els sufixos de
/// media (`/photo/1`, `/video/1`), que fxtwitter respondria amb 404. Suporta
/// també les formes sense usuari (`/i/status/…`, `/status/…`).
pub fn tweet_path(url: &str) -> Option<String> {
    if !is_twitter_url(url) {
        return None;
    }
    let after_scheme = url.trim().split_once("://").map(|(_, r)| r).unwrap_or(url.trim());
    let path = after_scheme.split_once('/').map(|(_, p)| p).unwrap_or("");
    let path = path.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let si = parts.iter().position(|p| *p == "status" || *p == "statuses")?;
    let id = parts.get(si + 1)?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // Sense usuari (x.com/status/ID) fxtwitter accepta el pseudo-handle `i`.
    let user = match si.checked_sub(1).and_then(|i| parts.get(i)) {
        Some(u) => *u,
        None => "i",
    };
    if !user.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(format!("{user}/status/{id}"))
}

/// Consulta l'API pública de fxtwitter i en retorna la fitxa del twitt.
/// `None` si la URL no és d'un twitt, si l'API falla o si el twitt no existeix.
pub async fn fetch_tweet_info(http: &reqwest::Client, url: &str) -> Option<TweetInfo> {
    let path = tweet_path(url)?;
    let api = format!("https://api.fxtwitter.com/{path}");
    let resp = http.get(&api).send().await.ok()?.error_for_status().ok()?;
    let v: Value = resp.json().await.ok()?;
    let info = TweetInfo::from_json(&v);
    if info.is_none() {
        tracing::debug!(%url, "social: fxtwitter sense twitt (esborrat o 404)");
    }
    info
}

/// Si la URL és d'una xarxa coneguda, n'extreu el contingut. Retorna `Ok(None)`
/// si no aplica o si l'API falla (perquè el caller faci el fetch normal).
pub async fn extract(http: &reqwest::Client, url: &str) -> Result<Option<Parsed>> {
    if is_twitter_url(url) {
        return Ok(extract_tweet(http, url).await);
    }
    let u = url.to_lowercase();
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

/// X/Twitter via fxtwitter (JSON públic amb el text i les dades del tweet).
async fn extract_tweet(http: &reqwest::Client, url: &str) -> Option<Parsed> {
    let info = fetch_tweet_info(http, url).await?;
    let text = info.text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let title = social_title(&info.author_name, &info.author_screen);
    tracing::info!(%url, "social: tweet extret via fxtwitter");
    Some(Parsed { title, text, og_type: None, image: info.image() })
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
    Some(Parsed { title, text, og_type: None, image: None })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_twitter_url_matches_hosts_not_substrings() {
        assert!(is_twitter_url("https://x.com/u/status/1"));
        assert!(is_twitter_url("https://twitter.com/u/status/1"));
        assert!(is_twitter_url("https://mobile.twitter.com/u/status/1"));
        assert!(is_twitter_url("https://www.x.com/u/status/1"));
        assert!(is_twitter_url("HTTPS://X.COM/u/status/1"));
        // Falsos positius de substring: no són X.
        assert!(!is_twitter_url("https://ex.com/u/status/1"));
        assert!(!is_twitter_url("https://notx.com/u/status/1"));
        assert!(!is_twitter_url("https://x.com.evil.example/u/status/1"));
        assert!(!is_twitter_url("https://bsky.app/profile/a/post/1"));
    }

    #[test]
    fn tweet_path_normalizes_media_suffixes_and_queries() {
        let id = "2083749667410727319";
        for u in [
            format!("https://x.com/karpathy/status/{id}"),
            format!("https://twitter.com/karpathy/status/{id}"),
            format!("https://x.com/karpathy/status/{id}/photo/1"),
            format!("https://x.com/karpathy/status/{id}/video/1"),
            format!("https://twitter.com/karpathy/status/{id}?s=20&t=abc"),
            format!("https://mobile.twitter.com/karpathy/status/{id}"),
        ] {
            assert_eq!(tweet_path(&u).as_deref(), Some(format!("karpathy/status/{id}").as_str()), "{u}");
        }
        // Sense usuari -> pseudo-handle `i` (fxtwitter l'accepta).
        assert_eq!(tweet_path(&format!("https://x.com/i/status/{id}")).as_deref(), Some(format!("i/status/{id}").as_str()));
        assert_eq!(tweet_path(&format!("https://x.com/status/{id}")).as_deref(), Some(format!("i/status/{id}").as_str()));
        // No és un twitt: perfil, id no numèric, altre host.
        assert!(tweet_path("https://x.com/karpathy").is_none());
        assert!(tweet_path("https://x.com/karpathy/status/abc").is_none());
        assert!(tweet_path("https://bsky.app/profile/a/post/1").is_none());
    }

    #[test]
    fn tweet_info_parses_json_and_picks_media_image() {
        let v: Value = serde_json::from_str(
            r#"{"code":200,"message":"OK","tweet":{
                "text":"Hello world",
                "author":{"screen_name":"karpathy","name":"Andrej Karpathy",
                          "url":"https://x.com/karpathy","avatar_url":"https://pbs.twimg.com/a.jpg",
                          "followers":4470090},
                "likes":29790,"retweets":2311,"replies":1803,"quotes":753,
                "bookmarks":12127,"views":6323136,"lang":"en",
                "created_timestamp":1785639609,
                "media":{"all":[
                    {"type":"photo","url":"https://pbs.twimg.com/media/x.jpg?name=orig"},
                    {"type":"video","url":"https://video.twimg.com/v.mp4",
                     "thumbnail_url":"https://pbs.twimg.com/t.jpg"}
                ]}
            }}"#,
        )
        .unwrap();
        let info = TweetInfo::from_json(&v).expect("twitt");
        assert_eq!(info.text, "Hello world");
        assert_eq!(info.author_screen, "karpathy");
        assert_eq!(info.followers, 4_470_090);
        assert_eq!(info.views, 6_323_136);
        assert_eq!(info.created_timestamp, Some(1785639609));
        assert!(info.created_at.as_deref().unwrap_or("").starts_with("2026-08-02"));
        assert_eq!(info.media.len(), 2);
        // La imatge és la foto, no la miniatura del vídeo.
        assert_eq!(info.image().as_deref(), Some("https://pbs.twimg.com/media/x.jpg?name=orig"));
    }

    #[test]
    fn tweet_info_video_only_uses_thumbnail_and_404_is_none() {
        let v: Value = serde_json::from_str(
            r#"{"tweet":{"text":"gif","author":{"screen_name":"a","name":"A"},
                "media":{"all":[{"type":"gif","url":"https://video.twimg.com/x.mp4",
                "thumbnail_url":"https://pbs.twimg.com/x.jpg"}]}}}"#,
        )
        .unwrap();
        let info = TweetInfo::from_json(&v).unwrap();
        assert_eq!(info.image().as_deref(), Some("https://pbs.twimg.com/x.jpg"));

        // 404 de fxtwitter: `tweet` és null.
        let none: Value = serde_json::from_str(r#"{"code":404,"message":"NOT_FOUND","tweet":null}"#).unwrap();
        assert!(TweetInfo::from_json(&none).is_none());
    }
}
