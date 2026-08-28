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
    /// Imatge d'acompanyament: og:image de l'article o primera imatge.
    /// Només URLs absolutes http(s); se serveix proxied via /img.
    pub image: Option<String>,
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

    // Imatge d'acompanyament: og:image (o twitter:image), amb fallback a la
    // primera imatge de l'article (article img > a img > img).
    let image = meta_content(&doc, "property", "og:image")
        .or_else(|| meta_content(&doc, "name", "twitter:image"))
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .or_else(|| {
            Selector::parse("article img, a img, main img, img")
                .ok()
                .and_then(|sel| {
                    doc.select(&sel).find_map(|e| e.value().attr("src"))
                })
                .map(|s| s.trim().to_string())
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        });

    // Text: prioritza <article>, si no <p>.
    let text = extract_text(&doc);

    Parsed { title, text, og_type, image }
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

/// Obertes de metallenguatge que eliminem quan apareixen a l'inici d'un resum
/// del LLM: «L'anàlisi de l'article...», «L'article descriu...», «En aquest
/// vídeo...», «Aquest repositori...», labels tipus «Resum:»/«**Resum:**», i els
/// equivalents en castellà/anglès que els models de vegades barregen. L'objectiu
/// és que el resum comenci directament per la notícia (prosa periodística).
const META_OPENINGS: &[&str] = &[
    // Català. Ordre important: les variants amb verb (que treuen subjecte+verb)
    // han d'anar abans que el subjecte sol, perquè el primer match guanya.
    "l'anàlisi de l'article presenta",
    "l'anàlisi de l'article descriu",
    "l'anàlisi de l'article explica",
    "l'anàlisi de l'article analitza",
    "l'anàlisi de l'article resumeix",
    "l'anàlisi de l'article",
    "aquesta anàlisi presenta",
    "aquesta anàlisi descriu",
    "aquesta anàlisi explica",
    "aquesta anàlisi analitza",
    "aquesta anàlisi es dedica",
    "aquesta anàlisi",
    "l'article descriu",
    "l'article explica",
    "l'article analitza",
    "l'article resumeix",
    "l'article presenta",
    "l'article tracta",
    "l'article parla",
    "l'article aborda",
    "aquest article descriu",
    "aquest article explica",
    "aquest article analitza",
    "aquest article presenta",
    "aquest article tracta",
    "aquest article parla",
    "en aquest article",
    "aquest article",
    "el vídeo descriu",
    "el vídeo explica",
    "el vídeo analitza",
    "el vídeo tracta",
    "el vídeo parla",
    "en aquest vídeo",
    "aquest vídeo",
    "aquest video",
    "el vídeo",
    "el video",
    "el repositori descriu",
    "el repositori explica",
    "el repositori analitza",
    "el repositori conté",
    "el repositori ofereix",
    "aquest repositori",
    // Castellà / anglès (equivalents que alguns models generen)
    "el artículo analiza",
    "el artículo describe",
    "el artículo explica",
    "el artículo trata",
    "este artículo trata",
    "en este artículo",
    "este artículo",
    "este vídeo",
    "este video",
    "el vídeo analiza",
    "el vídeo describe",
    "el vídeo explica",
    "el repositorio ofrece",
    "el repositorio contiene",
    "el repositorio",
    "this article describes",
    "this article explains",
    "this article analyzes",
    "in this article",
    "this article",
    "the article describes",
    "the article explains",
    "the article analyzes",
    "this video describes",
    "this video explains",
    "this video",
    "the video describes",
    "the video explains",
    "this repository",
    "the repository",
];

/// Labels de format pur (capçaleres markdown) que no aporten informació i que
/// traiem si encapçalen un resum (p.ex. «## Resum», «**Resumen:**»).
const META_LABELS: &[&str] = &[
    "resum",
    "resumen",
    "anàlisi",
    "anàlisis",
    "síntesi",
    "sintesi",
    "nota",
];

/// Neteja un resum del LLM perquè comenci directament per la notícia: elimina
/// capçaleres/labels de format inicials («## Resum», «**Resum:**», llistes
/// buides...) i frases metalingüístiques del tipus «L'article descriu...»,
/// «Aquesta anàlisi...». Només actua a l'inici i només sobre text que *presenta*
/// el contingut: la resta del text no s'edita (fidelitat al contingut).
pub fn polish_summary(s: &str) -> String {
    let mut out = s.trim().to_string();
    // Uns quants passos: les obertures poden estar encadenades o embolicades amb
    // format (`**Resum:** L'article descriu que ...`).
    for _ in 0..10 {
        let before = out.clone();
        out = strip_leading_meta(&out);
        if out == before {
            break;
        }
    }
    out
}

/// Treu marques de markdown (capçalera/llista/negreta) dels extrems d'una línia.
fn unwrap_md(s: &str) -> &str {
    s.trim()
        .trim_start_matches(|c: char| matches!(c, '#' | '-' | '*' | '+' | '>' | '•' | '·'))
        .trim_end_matches('*')
        .trim()
}

/// Elimina la primera obertura metalingüística (una línia de format, una label
/// tipus «Resum: X», o un prefix com «L'article descriu que X») i re-capitalitza
/// el que segueix. Si no n'hi ha cap, retorna el text sense canvis.
fn strip_leading_meta(s: &str) -> String {
    let s = s.trim_start();
    let first = s.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return String::new();
    }

    // Desembolica marques markdown del començament de la primera línia.
    let unwrapped = unwrap_md(first);
    let bare = unwrapped.trim_end_matches(':').trim();

    // (a) Línia de format pur o label sola («## Resum», «- », «**Resum:**»):
    //     es treu i es continua amb la resta del text.
    if bare.is_empty() || META_LABELS.contains(&bare.to_lowercase().as_str()) {
        return s[first.len()..].trim().to_string();
    }

    // (b) Label encapçalant contingut a la mateixa línia («Resum: X»,
    //     «## Resum: X», «**Resumen:** X»).
    if let Some(tail) = meta_label_tail(unwrapped) {
        return recapitalize(&(tail + &s[first.len()..])).trim().to_string();
    }

    // (c) Prefix de metallenguatge («L'article descriu que X»).
    let lower: String = unwrapped.to_lowercase().replace('’', "'");
    for p in META_OPENINGS {
        if lower.starts_with(p) {
            let rest = unwrapped[p.len()..].trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, ':' | '-' | '—' | '–' | '.' | '*' | ',')
            });
            // «descriu que X» -> «X» (la conjunció lligava amb el subjecte esborrat).
            let rest = match rest.strip_prefix("que") {
                Some(r) => r.trim_start(),
                None => rest,
            };
            return recapitalize(&(rest.to_string() + &s[first.len()..]))
                .trim()
                .to_string();
        }
    }
    s.to_string()
}

/// Comprova que `s` comenci pel label ignorant majúscules/minúscules (i accents
/// bàsics) i retorna la resta de `s` (en la seva caixa original, sense tocar).
fn strip_label_icase<'a>(s: &'a str, lbl: &str) -> Option<&'a str> {
    let mut s_chars = s.char_indices();
    let mut end = 0;
    for lc in lbl.chars() {
        let (idx, sc) = s_chars.next()?;
        match (sc.to_lowercase().next(), lc.to_lowercase().next()) {
            (Some(a), Some(b)) if a == b => end = idx + sc.len_utf8(),
            _ => return None,
        }
    }
    Some(&s[end..])
}

/// Si el text comença amb una label de meta seguida d'un separador i contingut
/// (p.ex. «resum: el mercat...»), retorna el contingut que segueix el separador.
fn meta_label_tail(unwrapped: &str) -> Option<String> {
    for lbl in META_LABELS {
        let Some(rest) = strip_label_icase(unwrapped, lbl) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(body) = rest
            .strip_prefix(':')
            .or_else(|| rest.strip_prefix(" - "))
            .or_else(|| rest.strip_prefix('-'))
            .or_else(|| rest.strip_prefix('·'))
            .or_else(|| rest.strip_prefix('.'))
            .or_else(|| rest.strip_prefix('—'))
            .or_else(|| rest.strip_prefix('–'))
        else {
            continue;
        };
        let body = unwrap_md(body);
        if body.is_empty() {
            continue;
        }
        return Some(body.to_string());
    }
    None
}

/// Posa en majúscula la primera lletra d'un text (la resta no es toca).
fn recapitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
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
        Ok((title, link_type, analysis, image)) => {
            db.update_link_analysis(link_id, title.as_deref(), link_type, &analysis, image.as_deref())
                .await?;
            // Baixa la imatge d'acompanyament i la desa LOCALMENT (data/images)
            // perquè l'overlay no depengui del servidor original a l'aire.
            // Best-effort: si falla, l'overlay farà servir el proxy remot /img.
            if let Some(img_url) = image {
                if let Err(e) = cache_link_image(db, cfg, http, link_id, &img_url).await {
                    tracing::warn!(%link_id, error = %e, "no s'ha pogut desar la imatge localment (es farà servir el proxy /img)");
                }
            }
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

/// Extensió de fitxer per a una imatge, a partir del seu `Content-Type`.
/// Només accepta tipus d'imatge; `None` per a qualsevol altra cosa (per
/// no desar HTML/massacres que un servidor pugui enviar amb status 200).
fn image_ext(ct: &str) -> Option<&'static str> {
    let base = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match base.as_str() {
        "image/jpeg" => Some(".jpg"),
        "image/png" => Some(".png"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        "image/avif" => Some(".avif"),
        "image/svg+xml" => Some(".svg"),
        _ => None,
    }
}

/// Baixa la imatge d'acompanyament d'un link i la desa a `cfg.images_dir`.
/// Retorna el nom de fitxer desat (p.ex. `<id>.jpg`). Best-effort: qualsevol
/// error (SSRF, xarxa, massa gran, tipus no-imatge) es propaga i el cridant
/// decideix caure al proxy remot.
pub(crate) async fn cache_link_image(
    db: &Db,
    cfg: &Config,
    http: &reqwest::Client,
    link_id: Uuid,
    image_url: &str,
) -> Result<Option<String>> {
    let (bytes, ct) = crate::overlay::fetch_image(http, image_url).await?;
    let Some(ext) = image_ext(&ct) else {
        return Ok(None); // no és una imatge; deixem que faci servir el proxy
    };
    let name = format!("{link_id}{ext}");
    let dir = std::path::Path::new(&cfg.images_dir);
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(&name), &bytes)?;
    db.set_link_image_file(link_id, Some(&name)).await?;
    tracing::info!(%link_id, %image_url, bytes = bytes.len(), "imatge desada localment: {name}");
    Ok(Some(name))
}

async fn run_inner(
    cfg: &Config,
    http: &reqwest::Client,
    llm: Option<&LlmClient>,
    url: &str,
) -> Result<(Option<String>, LinkType, Analysis, Option<String>)> {
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
            Ok(mut a) => {
                // Prosa periodística de sortida: treu els opens metalingüístics
                // («L'article descriu...», «## Resum»...) que de vegades genera.
                a.summary = polish_summary(&a.summary);
                a
            }
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

    Ok((final_title, link_type, analysis, parsed.image))
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

    #[test]
    fn image_ext_maps_known_types_and_rejects_others() {
        assert_eq!(image_ext("image/jpeg"), Some(".jpg"));
        assert_eq!(image_ext("image/png"), Some(".png"));
        assert_eq!(image_ext("image/webp"), Some(".webp"));
        // Suffix del paràmetre charset s'ignora.
        assert_eq!(image_ext("image/png; charset=binary"), Some(".png"));
        // Majúscules normalitzades.
        assert_eq!(image_ext("IMAGE/PNG"), Some(".png"));
        // No-imatges -> None (no desar HTML/JSON amb status 200).
        assert_eq!(image_ext("text/html"), None);
        assert_eq!(image_ext(""), None);
        assert_eq!(image_ext("application/octet-stream"), None);
    }

    #[test]
    fn polish_article_opening_is_removed() {
        // Els opens metalingüístics típics del LLM desapareixen i la notícia
        // ja comença directament (prosa periodística), re-capitalitzada.
        assert_eq!(
            polish_summary("L'article descriu com OpenAI ha publicat el seu nou model."),
            "Com OpenAI ha publicat el seu nou model."
        );
        assert_eq!(
            polish_summary("L'anàlisi de l'article presenta les claus de la crisi energètica."),
            "Les claus de la crisi energètica."
        );
        assert_eq!(
            polish_summary("L'article explica que els preus han pujat un 5%."),
            "Els preus han pujat un 5%."
        );
        assert_eq!(
            polish_summary("En aquest vídeo, el canal analitza la nova versió del compilador."),
            "El canal analitza la nova versió del compilador."
        );
        // La 'que' relativa lligada al subjecte esborrat també es treu.
        assert_eq!(
            polish_summary("l'article descriu que els preus han pujat."),
            "Els preus han pujat."
        );
    }

    #[test]
    fn polish_strips_leading_labels_and_markdown() {
        // Capçaleres/labels de format inicials s'eliminen, encadenades.
        assert_eq!(
            polish_summary("## Resum\n\n**L'article descriu** que plou a Barcelona."),
            "Plou a Barcelona."
        );
        assert_eq!(polish_summary("**Resum:** Els mercats tanquen a la baixa."), "Els mercats tanquen a la baixa.");
        assert_eq!(polish_summary("Resumen: La UE aprova el paquet de mesures."), "La UE aprova el paquet de mesures.");
        // '- ' buit inicial també es treu.
        assert_eq!(polish_summary("- \nLa setmana comença tranquil·la."), "La setmana comença tranquil·la.");
    }

    #[test]
    fn polish_keeps_plain_prose_untouched() {
        // Un resum que ja comença directament per la notícia no es modifica.
        let s = "La nova llei entrarà en vigor al gener, segons el govern.";
        assert_eq!(polish_summary(s), s);
        let s2 = "Els desenvolupadors de Rust publiquen la versió 1.80.";
        assert_eq!(polish_summary(s2), s2);
        // Text buit o només metallenguatge no peta.
        assert_eq!(polish_summary(""), "");
        assert_eq!(polish_summary("L'article descriu"), "");
    }

    #[test]
    fn polish_does_not_mangle_real_labels_or_other_languages() {
        // «Resumo:» portuguès no és un label nostre: s'ha de deixar igual
        // (una "o" després de "resum" no ha de caure mai a dins de «resum»).
        assert_eq!(
            polish_summary("Resumo: A UE aproba o paquete de medidas."),
            "Resumo: A UE aproba o paquete de medidas."
        );
        // Content headings / etiquetes seguides de text no es toquen.
        assert_eq!(
            polish_summary("Nota final: el mercat obre a la baixa."),
            "Nota final: el mercat obre a la baixa."
        );
        assert_eq!(
            polish_summary("Resum dels 253 patrons de disseny urbà."),
            "Resum dels 253 patrons de disseny urbà."
        );
        // Sí que s'eliminen la label «Nota:» i «Resum:» pures al davant.
        assert_eq!(
            polish_summary("Nota: el mercat obre a la baixa."),
            "El mercat obre a la baixa."
        );
    }
}
