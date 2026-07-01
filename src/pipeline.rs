use crate::config::Config;
use crate::db::Db;
use crate::error::{AppError, Result};
use crate::embed::Embedder;
use crate::llm::LlmClient;
use crate::models::{Analysis, LinkType, Sentiment};
use scraper::{Html, Selector};
use std::sync::Arc;
use uuid::Uuid;

/// Contingut extret d'una pagina.
pub struct Parsed {
    pub title: Option<String>,
    pub text: String,
    pub og_type: Option<String>,
}

/// FETCH: descarrega HTML amb capçaleres de navegador, timeout i limit de mida.
///
/// Si el servidor respon amb un mur anti-bot (403/429/503) i hi ha un
/// FlareSolverr configurat (`cfg.flaresolverr_url`), reintenta la descàrrega a
/// través seu (navegador headless que resol el challenge de Cloudflare & co.).
pub async fn fetch(http: &reqwest::Client, cfg: &Config, url: &str) -> Result<String> {
    let max_bytes = cfg.max_link_size_bytes;
    // Capçaleres que imiten un navegador real: molts murs bloquegen només per la
    // forma de les capçaleres (UA de bot, falta d'Accept, etc.). El UA ja el posa
    // el client compartit. No fixem Accept-Encoding: reqwest es compila sense
    // gzip/brotli i no en descomprimiria el cos.
    let resp = http
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8")
        .header("Accept-Language", "ca,es;q=0.9,en;q=0.8")
        .header("Upgrade-Insecure-Requests", "1")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Site", "none")
        .header("Sec-Fetch-User", "?1")
        .send()
        .await?;

    let status = resp.status();
    if matches!(status.as_u16(), 403 | 429 | 503) {
        if let Some(base) = cfg.flaresolverr_url.as_deref() {
            tracing::info!(%url, %status, "fetch bloquejat, reintent via FlareSolverr");
            return crate::flaresolverr::fetch(base, url, cfg.flaresolverr_timeout_secs, max_bytes)
                .await;
        }
    }

    let resp = resp.error_for_status()?;
    if let Some(len) = resp.content_length() {
        if len as usize > max_bytes {
            return Err(AppError::Pipeline(format!("body too large: {len} bytes")));
        }
    }
    let bytes = resp.bytes().await?;
    if bytes.len() > max_bytes {
        return Err(AppError::Pipeline(format!("body too large: {} bytes", bytes.len())));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// PARSE: extreu titol, text net i og:type.
pub fn parse(html: &str) -> Parsed {
    let doc = Html::parse_document(html);

    let og_title = meta_content(&doc, "property", "og:title");
    let title_tag = Selector::parse("title").ok().and_then(|sel| {
        doc.select(&sel).next().map(|e| e.text().collect::<String>().trim().to_string())
    });
    let title = og_title
        .or(title_tag)
        .filter(|s| !s.is_empty());

    let og_type = meta_content(&doc, "property", "og:type");

    // Text: prioritza <article>, si no <p>.
    let text = extract_text(&doc);

    Parsed { title, text, og_type }
}

fn meta_content(doc: &Html, attr: &str, value: &str) -> Option<String> {
    let sel = Selector::parse(&format!(r#"meta[{attr}="{value}"]"#)).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|e| e.value().attr("content"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn extract_text(doc: &Html) -> String {
    let article_sel = Selector::parse("article p, main p").unwrap();
    let mut parts: Vec<String> = doc
        .select(&article_sel)
        .map(|e| e.text().collect::<String>())
        .collect();
    if parts.is_empty() {
        let p_sel = Selector::parse("p").unwrap();
        parts = doc.select(&p_sel).map(|e| e.text().collect::<String>()).collect();
    }
    let joined = parts.join("\n");
    joined.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// CLASSIFY: heuristica per tipus d'enllaç.
pub fn classify(url: &str, og_type: Option<&str>) -> LinkType {
    let u = url.to_lowercase();
    if u.contains("github.com") || u.contains("gitlab.com") || u.contains("bitbucket.org") {
        return LinkType::Repo;
    }
    if u.contains("youtube.com") || u.contains("youtu.be") || u.contains("vimeo.com") {
        return LinkType::Video;
    }
    // Xarxes socials: auth-walled, no fem deep; classifiquem per filtre/icona.
    // Match per host (no substring: "ex.com" no és "x.com").
    const SOCIAL: &[&str] = &[
        "instagram.com", "tiktok.com", "twitter.com", "x.com",
        "threads.net", "facebook.com", "linkedin.com",
    ];
    if SOCIAL.iter().any(|d| host_is(&u, d)) {
        return LinkType::Social;
    }
    if let Some(t) = og_type {
        if t.contains("article") {
            return LinkType::Article;
        }
        if t.contains("video") {
            return LinkType::Video;
        }
    }
    if u.contains("/blog/") || u.contains("medium.com") || u.contains(".blog") {
        return LinkType::Blog;
    }
    if u.contains("/news/") {
        return LinkType::News;
    }
    LinkType::Other
}

/// Cert si el host de `url` (ja en minúscules) és `domain` o un subdomini seu.
/// Evita falsos positius de substring (p.ex. "ex.com" vs "x.com").
fn host_is(url: &str, domain: &str) -> bool {
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    host == domain || host.ends_with(&format!(".{domain}"))
}

// ---- Fallback heuristic (sense LLM) ----

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "una", "uns", "les", "els", "del",
    "que", "amb", "per", "una", "dels", "han", "the", "are", "was", "his", "her", "els", "des",
    "com", "mes", "the", "you", "your", "but", "not", "all", "can", "has", "have", "els", "una",
];

fn first_sentences(text: &str, n: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for ch in text.chars() {
        out.push(ch);
        if ch == '.' || ch == '!' || ch == '?' {
            count += 1;
            if count >= n {
                break;
            }
        }
    }
    out.trim().to_string()
}

fn heuristic_tags(title: &str, text: &str) -> Vec<String> {
    use std::collections::HashMap;
    let mut freq: HashMap<String, u32> = HashMap::new();
    let source = format!("{title} {title} {text}"); // pondera el titol
    for word in source.split(|c: char| !c.is_alphanumeric()) {
        let w = deaccent(&word.to_lowercase());
        if w.len() < 4 || STOPWORDS.contains(&w.as_str()) {
            continue;
        }
        *freq.entry(w).or_insert(0) += 1;
    }
    let mut items: Vec<(String, u32)> = freq.into_iter().collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    items.into_iter().take(8).map(|(w, _)| w).collect()
}

fn deaccent(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ä' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

fn heuristic_sentiment(text: &str) -> Sentiment {
    let pos = ["bo", "bon", "excel", "millor", "great", "good", "love", "wonderful", "exit"];
    let neg = ["dolent", "pitjor", "bad", "hate", "terrible", "error", "fail", "problema", "crisi"];
    let lower = deaccent(&text.to_lowercase());
    let p = pos.iter().filter(|w| lower.contains(*w)).count() as i32;
    let n = neg.iter().filter(|w| lower.contains(*w)).count() as i32;
    if p > n {
        Sentiment::Positive
    } else if n > p {
        Sentiment::Negative
    } else {
        Sentiment::Neutral
    }
}

fn heuristic_analysis(title: &str, text: &str, max_chars: usize) -> Analysis {
    let mut summary = first_sentences(text, 3);
    if summary.is_empty() {
        summary = title.to_string();
    }
    // limita per caràcters, respectant el límit de paraula
    if summary.chars().count() > max_chars {
        let truncated: String = summary.chars().take(max_chars).collect();
        summary = match truncated.rfind(' ') {
            Some(i) if i >= max_chars / 2 => truncated[..i].to_string(),
            _ => truncated.trim_end().to_string(),
        };
    }
    Analysis {
        title: None,
        summary,
        tags: heuristic_tags(title, text),
        sentiment: heuristic_sentiment(text),
    }
}

/// Retalla un títol a ~80 caràcters respectant límits de paraula.
pub fn clamp_title(s: &str) -> String {
    const MAX: usize = 80;
    let t = s.trim();
    if t.chars().count() <= MAX {
        return t.to_string();
    }
    let truncated: String = t.chars().take(MAX).collect();
    let cut = match truncated.rfind(' ') {
        Some(i) if i >= MAX / 2 => &truncated[..i],
        _ => truncated.trim_end(),
    };
    format!("{}…", cut.trim_end_matches(['.', ',', ' ', '-', ':']))
}

/// Pipeline complet per a un link. Actualitza la DB.
pub async fn process_link(
    db: &Db,
    cfg: &Config,
    http: &reqwest::Client,
    llm: Option<&LlmClient>,
    embedder: Option<&Embedder>,
    link_id: Uuid,
) -> Result<()> {
    let link = db.link_by_id(link_id).await?.ok_or(AppError::NotFound)?;
    db.set_link_status(link_id, crate::models::LinkStatus::Processing).await?;

    let result = run_inner(cfg, http, llm, &link.url).await;

    match result {
        Ok((title, link_type, analysis)) => {
            db.update_link_analysis(link_id, title.as_deref(), link_type, &analysis).await?;
            // Embedding semàntic per al ranking personalitzat (best-effort).
            if let Some(emb) = embedder {
                let text = embed_source(title.as_deref(), &analysis);
                if let Err(e) = embed_and_store(db, emb, link_id, &text).await {
                    tracing::warn!(%link_id, error = %e, "embedding failed");
                }
            }
            tracing::info!(%link_id, url = %link.url, "processed");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(%link_id, url = %link.url, error = %e, "processing failed");
            db.set_link_status(link_id, crate::models::LinkStatus::Failed).await?;
            Err(e)
        }
    }
}

async fn run_inner(
    cfg: &Config,
    http: &reqwest::Client,
    llm: Option<&LlmClient>,
    url: &str,
) -> Result<(Option<String>, LinkType, Analysis)> {
    // Xarxes socials (X/Bluesky) renderitzen amb JS: un GET només dóna un mur de
    // login. Provem un extractor d'API pública abans del fetch genèric.
    let parsed = match crate::social::extract(http, url).await? {
        Some(p) => p,
        None => {
            let html = fetch(http, cfg, url).await?;
            parse(&html)
        }
    };
    let link_type = classify(url, parsed.og_type.as_deref());

    let title = parsed.title.clone().unwrap_or_default();
    let text_trunc: String = parsed.text.chars().take(4000).collect();

    let analysis = match llm {
        Some(client) => match client.analyze(&title, &text_trunc, cfg.summary_max_chars).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "llm failed, using heuristic fallback");
                heuristic_analysis(&title, &parsed.text, cfg.summary_max_chars)
            }
        },
        None => heuristic_analysis(&title, &parsed.text, cfg.summary_max_chars),
    };

    // Títol: prioritza el curt del LLM; si no, retalla el de la pàgina a ~80 car.
    let final_title = analysis
        .title
        .clone()
        .or_else(|| parsed.title.clone())
        .map(|t| clamp_title(&t))
        .filter(|t| !t.is_empty());

    Ok((final_title, link_type, analysis))
}

// ---- Embeddings ----

/// Text font de l'embedding: títol + resum + tags (senyal semàntic compacte).
fn embed_source(title: Option<&str>, a: &Analysis) -> String {
    format!("{}\n{}\n{}", title.unwrap_or(""), a.summary, a.tags.join(" "))
}

/// L2-normalitza (perquè el centroide de "cors" ponderi cada link igual) i
/// quantitza a int8 simètric. Retorna (vec_i8, scale) on f32 ≈ i8 * scale.
pub fn quantize(v: &[f32]) -> Option<(Vec<i8>, f32)> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return None;
    }
    let scale = 1.0 / 127.0; // vector normalitzat => |x| ≤ 1
    let q: Vec<i8> = v
        .iter()
        .map(|&x| ((x / norm) / scale).round().clamp(-127.0, 127.0) as i8)
        .collect();
    Some((q, scale))
}

/// Genera i desa l'embedding d'un link (best-effort, requereix embedder actiu).
pub async fn embed_and_store(
    db: &Db,
    embedder: &Embedder,
    link_id: Uuid,
    text: &str,
) -> Result<()> {
    let v = embedder.embed(text).await?;
    if let Some((q, scale)) = quantize(&v) {
        db.update_link_embedding(link_id, &q, scale).await?;
    }
    Ok(())
}

/// Construeix el client LLM si esta configurat.
pub fn build_llm(cfg: &Config, http: reqwest::Client) -> Option<Arc<LlmClient>> {
    if cfg.llm.enabled() {
        Some(Arc::new(LlmClient::new(http, cfg.llm.clone())))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_repo() {
        assert_eq!(classify("https://github.com/x/y", None), LinkType::Repo);
        assert_eq!(classify("https://youtu.be/abc", None), LinkType::Video);
        assert_eq!(classify("https://ex.com", Some("article")), LinkType::Article);
        assert_eq!(classify("https://www.instagram.com/p/abc", None), LinkType::Social);
        assert_eq!(classify("https://x.com/u/status/1", None), LinkType::Social);
        assert_eq!(classify("https://ex.com", None), LinkType::Other);
    }

    #[test]
    fn parse_title_and_text() {
        let html = r#"<html><head><title>Hello</title>
            <meta property="og:type" content="article"></head>
            <body><article><p>First sentence. Second one.</p></article></body></html>"#;
        let p = parse(html);
        assert_eq!(p.title.as_deref(), Some("Hello"));
        assert!(p.text.contains("First sentence"));
        assert_eq!(p.og_type.as_deref(), Some("article"));
    }

    #[test]
    fn heuristic_first_sentences() {
        let a = heuristic_analysis("T", "One. Two. Three. Four.", 300);
        assert_eq!(a.summary, "One. Two. Three.");
    }

    #[test]
    fn quantize_preserves_direction() {
        // Vector zero => None.
        assert!(quantize(&[0.0, 0.0, 0.0]).is_none());

        // Dequantitzat ha de quedar prop de la direcció normalitzada.
        let v = vec![3.0f32, 4.0, 0.0]; // norm 5 => unit (0.6,0.8,0)
        let (q, s) = quantize(&v).unwrap();
        let deq: Vec<f32> = q.iter().map(|&x| x as f32 * s).collect();
        assert!((deq[0] - 0.6).abs() < 0.02);
        assert!((deq[1] - 0.8).abs() < 0.02);
        assert!(deq[2].abs() < 0.02);
    }
}
