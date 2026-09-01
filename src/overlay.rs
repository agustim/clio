//! Overlay de directe per a Twitch & co. (emissió headless 24/7).
//!
//! Genera una escena HTML autocontinguda (sense dependències externes) amb:
//!   - Bloc superior: logo + nom, i a la dreta rellotge/data en directe + "EN DIRECTE".
//!   - Zona central: una fila de cards de notícies que roten automàticament
//!     (títol, resum, tipus, font/@reporter, i imatge d'acompanyament amb crèdit).
//!   - Bloc inferior: news crawl (cinta de titulars) amb el nom de la font.
//!
//! Les dades arriben per `/overlay/ticker.json` (les últimes N notícies
//! processades) i l'escena les re-carrega periòdicament. Les imatges es
//! serveixen proxied via `/img?u=...` (evita hotlink, amaga el referrer i
//! evita bloquejos per referrer de tercers).
//!
//! Emissió: OBS amb Browser Source apuntant a `/overlay`, o el mode sense
//! OBS (Xvfb + Chromium + ffmpeg -> RTMP) descrit a `Dockerfile.stream` /
//! `scripts/stream.sh`.

use crate::config::Config;
use crate::db::Db;
use crate::error::{AppError, Result};
use crate::models::Link;
use serde_json::{json, Value};

/// Mida màxima (bytes) d'imatge que deixa passar el proxy.
const IMG_MAX_BYTES: usize = 6 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Dades per al ticker
// ---------------------------------------------------------------------------

/// Punt d'entrada de dades de l'escena: les últimes `max` notícies processades.
pub async fn ticker(db: &Db, max: usize) -> Result<Value> {
    let links = db.latest_done_links(max as i64).await?;
    let items: Vec<Value> = links.iter().map(item).collect();
    Ok(json!({
        "generated_at": crate::db::now_str(),
        "count": items.len(),
        "items": items,
    }))
}

/// Un element lleuger i autocontingut per a l'escena.
fn item(l: &Link) -> Value {
    let domain = l
        .url
        .split('/')
        .nth(2)
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_string();
    let source = l
        .reporters
        .first()
        .map(|r| format!("@{r}"))
        .unwrap_or_else(|| domain.clone());
    json!({
        "id": l.id.to_string(),
        "url": l.url,
        "domain": domain,
        "title": l.title.clone().unwrap_or_else(|| l.url.clone()),
        "summary": l.summary.clone().unwrap_or_default(),
        // Anàlisi profunda del LLM (text netejat de markdown). És el cos de la
        // notícia a l'escena; el nom de camp és "text" perquè la core ja el
        // mostra en lloc del resum curt quan existeix.
        "text": l.deep_summary.as_deref().map(clean_prose).unwrap_or_default(),
        "tags": l.tags,
        "link_type": l.link_type.as_str(),
        "sentiment": l.sentiment.as_str(),
        "source": source,
        "image": image_for(l),
        // MP3 de la veu del titular (si en té). La core el llegeix en veu
        // alta dins del grup de cards; si buit, salta aquesta notícia.
        "audio": l.audio_file.as_ref().map(|_| format!("/audio/{}", l.id)),
        // Dia i hora de la notícia (quan es va recollir). S'envia com a RFC3339
        // (UTC) i el navegador el mostra en hora local de l'emissió, com el rellotge.
        "date": l.created_at.to_rfc3339(),
    })
}

/// URL de la imatge d'una card: la còpia LOCAL (/imgout/{id}) si n'hi ha
/// (baixada en analitzar la cua) i, si no, el proxy remot (/img?u=...).
fn image_for(l: &Link) -> Option<String> {
    if l.image_file.is_some() {
        Some(format!("/imgout/{}", l.id))
    } else {
        l.image_url.as_deref().map(proxy_url)
    }
}

/// Neteja lleugera del text del LLM (markdown -> prosa per a l'overlay): treu
/// marques de títol/llista/negreta (`#`, `-`, `*`, `>`, `•`), línies buides i
/// col·lapsa les frases en un sol paràgraf continu.
fn clean_prose(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for raw in s.lines() {
        let l = raw.trim();
        if l.starts_with('>') {
            continue;
        }
        let mut chars = l.chars().peekable();
        // Treu prefix de llista/títol (també l'espai que el segueix).
        while chars
            .peek()
            .map(|c| matches!(c, '#' | '-' | '*' | '+' | '•' | '·'))
            .unwrap_or(false)
        {
            chars.next();
            while chars.peek().map(|c| c.is_whitespace()).unwrap_or(false) {
                chars.next();
            }
        }
        // Treu asteriscos sobrants (possibles restes de **negreta**).
        let mut cleaned: String = chars.collect();
        cleaned = cleaned.replace('*', "").trim().to_string();
        if cleaned.is_empty() {
            continue;
        }
        lines.push(cleaned);
    }
    lines.join(" ")
}

/// URL proxied d'una imatge remota: `/img?u=<percent-encoded>`.
fn proxy_url(raw: &str) -> String {
    format!("/img?u={}", percent_encode(raw))
}

/// Percent-encoding mínim (reservats i no-ASCII -> %XX), per a `u` de /img.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Proxy d'imatges (anti-hotlink + anti-SSRF bàsic)
// ---------------------------------------------------------------------------

/// Cert si podem proxiejar aquesta URL. Guardes conservadores de PoC:
/// només http(s), host amb punt (no "localhost"), i cap IP literal ni ranges
/// privades/lloc-local (bloquegem intents d'scan intern). Per a ús personal
/// és suficient; si l'API es fa pública, cal tornar-lo més estricte.
fn proxy_allowed(raw: &str) -> bool {
    let rest = if let Some(r) = raw.strip_prefix("https://") {
        r
    } else if let Some(r) = raw.strip_prefix("http://") {
        r
    } else {
        return false;
    };
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .split('@')
        .last()
        .unwrap_or("")
        .trim_start_matches('[')
        .trim_end_matches(']');
    if host.is_empty() || host.contains(' ') {
        return false;
    }
    // IP literal (v4 o v6) -> refusa (no podem saber si és privada a ull).
    if host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    !is_private_host(host)
}

/// Hosts prohibits: localhost i ranges privades (impedeix escanejar la xarxa
/// interna a través del proxy).
fn is_private_host(host: &str) -> bool {
    let h = host.to_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    const PRIVATE: &[&str] = &["10.", "127.", "169.254.", "192.168."];
    if PRIVATE.iter().any(|p| h.starts_with(p)) {
        return true;
    }
    // 172.16.0.0/12 (172.16-172.31).
    if let Some(rest) = h.strip_prefix("172.") {
        if let Some((first, _)) = rest.split_once('.') {
            if let Ok(n) = first.parse::<u8>() {
                if (16..=31).contains(&n) {
                    return true;
                }
            }
        }
    }
    false
}

/// Baixa la imatge remota (best-effort) i en retorna els bytes + content-type.
pub async fn fetch_image(
    http: &reqwest::Client,
    raw: &str,
) -> Result<(Vec<u8>, String)> {
    if !proxy_allowed(raw) {
        return Err(AppError::BadRequest("url no permesa".into()));
    }
    let resp = http
        .get(raw)
        .header("Accept", "image/*")
        .send()
        .await?
        .error_for_status()?;
    if let Some(len) = resp.content_length() {
        if len > IMG_MAX_BYTES as u64 {
            return Err(AppError::BadRequest("imatge massa gran".into()));
        }
    }
    // Agafa el content-type abans de consumir el cos (bytes() mou `resp`).
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "image/jpeg".to_string());
    let bytes = resp.bytes().await?;
    if bytes.len() > IMG_MAX_BYTES {
        return Err(AppError::BadRequest("imatge massa gran".into()));
    }
    if bytes.is_empty() {
        return Err(AppError::BadRequest("imatge buida".into()));
    }
    Ok((bytes.to_vec(), ct))
}

// ---------------------------------------------------------------------------
// Escena HTML
// ---------------------------------------------------------------------------

/// HTML de l'escena, amb els valors de config injectats.
pub fn overlay_html(cfg: &Config) -> String {
    OVERLAY_HTML
        .replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
        .replace("{{REFRESH}}", &cfg.overlay_refresh_secs.to_string())
        .replace("{{ROTATE}}", &cfg.overlay_rotate_secs.to_string())
        .replace("{{CARDS}}", &cfg.overlay_cards.max(1).to_string())
        .replace("{{LINES}}", &cfg.overlay_text_lines.max(1).to_string())
        // Finestra (minuts) per marcar una notícia com a NOVA; 0 = desactivada.
        .replace("{{NEW_MIN}}", &cfg.overlay_new_minutes.to_string())
        // Veu dels titulars + música de fons de l'escena.
        .replace("{{MUSIC_VOL}}", &format!("{:.2}", cfg.overlay_music_volume))
        .replace("{{MUSIC_DUCK}}", &format!("{:.2}", cfg.overlay_music_duck))
        .replace("{{READ_GAP}}", &cfg.overlay_read_gap_ms.to_string())
        // TimeZone IANA (JSON-quoted) per a les hores de l'escena; "" = hora local.
        .replace(
            "{{TIMEZONE}}",
            &serde_json::json!(cfg.overlay_timezone.as_deref().unwrap_or("")).to_string(),
        )
}

const OVERLAY_HTML: &str = r##"<!DOCTYPE html>
<html lang="ca">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="google" content="notranslate">
<title>Clio · Directe</title>
<style>
  :root {
    --bg:#0b0e15; --panel:#121726; --card:#151c2a; --card2:#1a2233;
    --fg:#eef2fa; --muted:#9aa7bd; --faint:#5d6a77;
    --acc:#6aa8ff; --red:#ff5a5a; --ok:#45d49a; --border:#242f42;
  }
  * { box-sizing:border-box; margin:0; padding:0; }
  html,body { height:100%; }
  body {
    font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
    background: var(--bg); color: var(--fg); overflow:hidden; user-select:none;
  }
  .scene { display:flex; flex-direction:column; height:100vh; }

  /* ---- Bloc superior: logo + rellotge ---- */
  .top {
    flex:none; height:94px; display:flex; align-items:center; justify-content:space-between;
    padding:0 22px; background:linear-gradient(180deg, #111726, #0e1420);
    border-bottom:1px solid var(--border);
  }
  .brand { display:flex; align-items:center; gap:16px; }
  .brand .mark {
    display:grid; place-items:center; width:58px; height:58px; border-radius:15px;
    background:linear-gradient(135deg, var(--acc), #b06aff); font-size:26px; color:#fff;
    box-shadow:0 4px 18px rgba(106,168,255,.35); font-weight:700;
  }
  .brand .name { font-size:31px; font-weight:800; letter-spacing:-.01em; }
  .brand .name small { color:var(--muted); font-weight:500; font-size:16px; margin-left:10px; }
  .top-right { display:flex; align-items:center; gap:18px; }
  .live {
    display:inline-flex; align-items:center; gap:8px; font-size:15px; font-weight:700;
    letter-spacing:.08em; color:#fff; background:var(--red); padding:8px 14px; border-radius:999px;
  }
  .live .dotl { width:10px; height:10px; border-radius:50%; background:#fff; animation:blink 1.1s infinite; }
  @keyframes blink { 50% { opacity:.15; } }
  .clock { text-align:right; line-height:1.02; }
  .clock .t { font-size:46px; font-weight:800; font-variant-numeric: tabular-nums; }
  .clock .d { font-size:16px; color:var(--muted); text-transform:capitalize; }

  /* ---- Zona central: cards ---- */
  .main { flex:1; position:relative; display:flex; align-items:center; padding:16px 24px; min-height:0; }
  .strip {
    display:flex; gap:18px; width:100%; height:100%;
  }
  .card {
    flex:1 1 0; min-width:0; display:flex; flex-direction:column;
    background:linear-gradient(180deg, var(--card), var(--card2));
    border:1px solid var(--border); border-radius:18px; overflow:hidden;
    box-shadow:0 10px 30px rgba(0,0,0,.45); animation:fade .5s ease;
  }
  @keyframes fade { from { opacity:0; transform:translateY(8px);} to {opacity:1; transform:none;} }
  .thumb { position:relative; width:100%; height:clamp(110px, 17vh, 180px); flex:none; background:var(--panel); overflow:hidden; }
  .thumb img { width:100%; height:100%; object-fit:cover; display:block; }
  .thumb .ph {
    width:100%; height:100%; display:grid; place-items:center; font-size:44px;
    background:linear-gradient(135deg, #182136, #101625); color:var(--faint);
  }
  .credit {
    position:absolute; right:9px; bottom:9px; font-size:13px; color:#fff;
    background:rgba(0,0,0,.62); padding:3px 9px; border-radius:7px; backdrop-filter:blur(2px);
  }
  .badge-lt {
    position:absolute; left:9px; top:9px; font-size:13px; font-weight:700; text-transform:uppercase;
    letter-spacing:.05em; padding:4px 9px; border-radius:7px; background:rgba(0,0,0,.55); color:var(--ok);
  }
  .badge-new {
    position:absolute; right:9px; top:9px; font-size:13px; font-weight:800; letter-spacing:.05em;
    padding:4px 9px; border-radius:7px; background:var(--red); color:#fff;
    box-shadow:0 0 14px rgba(255,90,90,.55);
  }
  /* Notícia publicada fa menys de 30 minuts: marc vermell amb pols suau. */
  .card.new {
    border-color:var(--red);
    box-shadow:0 0 0 2px rgba(255,90,90,.5), 0 10px 30px rgba(0,0,0,.45);
    animation:fade .5s ease, pulse 2.2s ease-in-out infinite;
  }
  @keyframes pulse { 50% { box-shadow:0 0 0 4px rgba(255,90,90,.22), 0 10px 30px rgba(0,0,0,.45); } }
  .card-body { flex:1; display:flex; flex-direction:column; gap:9px; padding:14px 16px 15px; min-height:0; overflow:hidden; }
  .card h3 {
    font-size:24px; line-height:1.22; font-weight:800; letter-spacing:-.01em;
    display:-webkit-box; -webkit-line-clamp:3; -webkit-box-orient:vertical; overflow:hidden;
  }
  .card .sum {
    font-size:19px; line-height:1.42; color:var(--muted); font-weight:500;
    display:-webkit-box; -webkit-line-clamp:{{LINES}}; -webkit-box-orient:vertical; overflow:hidden;
  }
  .card .meta {
    margin-top:auto; display:flex; align-items:center; justify-content:space-between; gap:8px;
    font-size:15.5px; color:var(--faint);
  }
  .card .src { font-weight:700; color:var(--acc); font-size:17px; }
  .card .when {
    flex:none; display:inline-flex; align-items:center; gap:5px; font-size:14.5px;
    font-weight:600; color:var(--muted); background:rgba(0,0,0,.28);
    padding:3px 9px; border-radius:7px; letter-spacing:.01em;
  }

  /* ---- Bloc inferior: news crawl ---- */
  .bottom {
    flex:none; height:80px; display:flex; align-items:center; gap:20px;
    background:#0c111c; border-top:2px solid var(--red); overflow:hidden; padding:0 20px;
  }
  .crawl-tag {
    flex:none; font-size:16px; font-weight:800; letter-spacing:.1em; color:#fff;
    background:var(--red); padding:9px 14px; border-radius:9px;
  }
  .crawl-wrap { flex:1; overflow:hidden; position:relative; height:100%; display:flex; align-items:center; }
  .crawl-track { display:inline-flex; white-space:nowrap; will-change:transform; animation:crawl linear infinite; }
  /* La durada la fixa el JS segons el nombre d'ítems. */
  @keyframes crawl { from { transform:translateX(0); } to { transform:translateX(-50%); } }
  .crawl-item { display:inline-flex; align-items:center; gap:12px; font-size:22px; font-weight:600; padding:0 38px; }
  .crawl-item .csrc { color:var(--acc); font-weight:800; font-size:17px; }
  .crawl-item .ctime { color:var(--faint); font-weight:600; font-size:16px; }
  .crawl-item .cnew {
    font-size:13px; font-weight:800; letter-spacing:.05em; color:#fff;
    background:var(--red); padding:3px 8px; border-radius:6px;
  }
  .crawl-item .cdot { color:var(--muted); }
</style>
</head>
<body>
  <div class="scene">
    <header class="top">
      <div class="brand">
        <span class="mark">◆</span>
        <div class="name">CLIO<small>· NOTÍCIES 24/7 · v{{VERSION}}</small></div>
      </div>
      <div class="top-right">
        <span class="live"><span class="dotl"></span>EN DIRECTE</span>
        <div class="clock">
          <div class="t" id="ov-clock">--:--:--</div>
          <div class="d" id="ov-date">—</div>
        </div>
      </div>
    </header>

    <main class="main">
      <div class="strip" id="strip"></div>
    </main>

    <footer class="bottom">
      <span class="crawl-tag">ÚLTIMA HORA</span>
      <div class="crawl-wrap">
        <div class="crawl-track" id="crawl"></div>
      </div>
    </footer>
  </div>

<script>
const DATAV = Date.now();
const REFRESH = parseInt({{REFRESH}}, 10) * 1000 || 60000;
const ROTATE  = parseInt({{ROTATE}}, 10) * 1000 || 12000;
const CARDS   = Math.max(1, parseInt({{CARDS}}, 10) || 4);

// TimeZone configurable de l'escena (IANA, p.ex. "Europe/Andorra"); "" = hora
// local del navegador que emet el stream.
const TZ = {{TIMEZONE}};
const TZ_OPT = TZ ? { timeZone: TZ } : {};
// Una notícia es considera NOVA durant els primers N minuts des de la publicació;
// la finestra és configurable (OVERLAY_NEW_MINUTES, en minuts; 0 = desactivada).
const NEW_MIN = Math.max(0, {{NEW_MIN}});
const NEW_MS = NEW_MIN * 60 * 1000;

// Veu dels titulars + música de fons de l'escena (fitxer public/music.mp3,
// lliure de drets; OVERLAY_MUSIC_* en configura-la).
const MUSIC_VOL  = parseFloat({{MUSIC_VOL}});   // volum normal de música
const MUSIC_DUCK = parseFloat({{MUSIC_DUCK}});  // volum mentre es llegeixen titulars
const READ_GAP   = Math.max(0, parseInt({{READ_GAP}}, 10) || 400); // pausa entre titulars (ms)

let ITEMS = [];
let idx = 0;

const $ = id => document.getElementById(id);

// ---- Rellotge / data (respecta la TimeZone configurada) ----
function tick() {
  const now = new Date();
  $('ov-clock').textContent = now.toLocaleTimeString('ca-ES',
    Object.assign({ hour12:false }, TZ_OPT));
  $('ov-date').textContent = now.toLocaleDateString('ca-ES',
    Object.assign({ weekday:'short', day:'2-digit', month:'short' }, TZ_OPT));
}
setInterval(tick, 500); tick();

// ---- Cards (rotació) ----
function esc(s){ return (s||'').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }
function domainOf(l){ return (l.domain || '').replace(/^www\./,''); }
// Tipus de notícia amb etiqueta llegible en català.
const TYPE_LABELS = { news:'Notícia', repo:'Repo', article:'Article', video:'Vídeo',
                      blog:'Blog', social:'Xarxa social', other:'Altres' };
function typeLabel(t){ return TYPE_LABELS[t] || (t || 'Altres'); }
// Dia i hora de la notícia (RFC3339/UTC -> la TimeZone de l'escena), ex. "27 ago · 12:45".
function fmtDate(iso){
  if (!iso) return '';
  const d = new Date(iso);
  if (isNaN(d.getTime())) return '';
  return d.toLocaleDateString('ca-ES',
           Object.assign({ day:'2-digit', month:'short' }, TZ_OPT))
       + ' · ' + d.toLocaleTimeString('ca-ES',
           Object.assign({ hour:'2-digit', minute:'2-digit' }, TZ_OPT));
}
// Vertader si la notícia s'ha publicat fa menys de NEW_MS (marcada com a NOVA).
function isNew(l){
  if (!l || !l.date) return false;
  const t = new Date(l.date).getTime();
  if (isNaN(t)) return false;
  return (Date.now() - t) < NEW_MS;
}
function cardHtml(l) {
  const img = l.image
    ? `<img src="${esc(l.image)}" alt="" loading="eager" referrerpolicy="no-referrer">`
    : `<div class="ph">◆</div>`;
  const credit = l.image && domainOf(l) ? `<span class="credit">📷 ${esc(domainOf(l))}</span>` : '';
  const type = esc(typeLabel(l.link_type));
  const fresh = isNew(l);
  const badgeNew = fresh ? '<span class="badge-new">● NOU</span>' : '';
  // Cos de la notícia = anàlisi profunda del LLM; si no n'hi ha, resum curt.
  const body = (l.text && l.text.trim()) ? l.text : (l.summary || l.title);
  return `<article class="card${fresh ? ' new' : ''}">
    <div class="thumb">${img}${credit}<span class="badge-lt">${type}</span>${badgeNew}</div>
    <div class="card-body">
      <h3>${esc(l.title)}</h3>
      <p class="sum">${esc(body)}</p>
      <div class="meta">
        <span class="src">${esc(l.source)}</span>
        <span class="when" title="${esc(l.url)}">🗓 ${esc(fmtDate(l.date))}</span>
      </div>
    </div>
  </article>`;
}
function renderCards() {
  if (!ITEMS.length) return;
  const slice = [];
  for (let i = 0; i < CARDS; i++) slice.push(ITEMS[(idx + i) % ITEMS.length]);
  $('strip').innerHTML = slice.map(cardHtml).join('');
}
// Rotació: tota la graella avança de cop, saltant de CARDS en CARDS (no va
// lliscant d'un en un): cada minut/rotació es veuen X notícies completament noves.
setInterval(() => { idx = (idx + CARDS) % ITEMS.length; renderCards(); maybeReadGroup(); }, ROTATE);

// ---- Veu dels titulars (lectura per grups) + música de fons ----
// Música de fons: llarga, en bucle, lliure de drets (el mateix usuari la deixa
// a public/music.mp3). Baixa de volum mentre es llegeixen els titulars (ducking).
const MUSIC = new Audio('/music.mp3');
MUSIC.loop = true;
MUSIC.preload = 'auto';
MUSIC.volume = MUSIC_VOL;   // abans de cap play, per si mai arrenca a més volum
let musicOn = false;      // cert només quan la música ha arrencat de veritat
let reading = false;      // cert mentre s'està llegint un grup
let lastGroupKey = '';    // grup ja llegit (per no repetir-lo si torna seguit)
let audioQueue = [];      // MPs pendents de llegir en seqüència
let missingIds = [];      // ids del grup actual que encara no tenen veu (reintent)

// Autoplay robust: Chrome pot rebutjar el primer play() (política d'autoplay
// amb so). En lloc de rendir-se, ho reintentem cada pocs segons i també ens
// desbloquegem amb el primer gest de l'usuari (clic/tocar/tecla). Amb el flag
// --autoplay-policy=no-user-gesture-required (mode headless/stream.sh) el
// primer play ja funciona; la resta són xarxes de seguretat (OBS/navegador).
MUSIC.addEventListener('canplay', () => { musicOn = true; tryMusic(); });
MUSIC.addEventListener('error', () => { musicOn = false; });
function tryMusic(){
  if (MUSIC.paused) MUSIC.play().catch(() => { /* encara no permès; reintentarem */ });
}
function startMusic(){
  MUSIC.load();
  setInterval(tryMusic, 5000);
}
// Primer gest de l'usuari → desbloqueja música i veu (navegador normal).
function unlockAudio(){
  tryMusic();
  if (VOICE.src && VOICE.paused) VOICE.play().catch(() => {});
}
['pointerdown','keydown','touchstart'].forEach(ev =>
  window.addEventListener(ev, unlockAudio, { once:true, passive:true }));

// Suavitza la baixada/pujada de la música (1 cops/frame fins al volum objectiu).
function rampTo(target){
  const cur = MUSIC.volume;
  const step = (target > cur) ? 0.0012 : -0.0022;
  const next = step > 0 ? Math.min(target, cur + step) : Math.max(target, cur + step);
  MUSIC.volume = Math.round(next * 1000) / 1000;
  if (musicOn && Math.abs(MUSIC.volume - target) > 0.005) requestAnimationFrame(() => rampTo(target));
}
function duckForSpeech(voice){
  const target = voice ? MUSIC_DUCK : MUSIC_VOL;
  if (musicOn) requestAnimationFrame(() => rampTo(target));
}

// Cua de reproducció: un titular rere l'altre, amb la pausa configurada.
const VOICE = new Audio();
VOICE.preload = 'auto';
function playQueue(){
  if (!audioQueue.length){ reading = false; duckForSpeech(false); return; }
  const url = audioQueue.shift();
  reading = true;
  duckForSpeech(true);
  VOICE.src = url;
  const next = () => { VOICE.onended = VOICE.onerror = null; setTimeout(() => playQueue(), READ_GAP); };
  VOICE.onended = next;
  VOICE.onerror = next;
  VOICE.play().catch(next);
}

// Grup actualment visible a la graella (les CARDS que es mostren).
function visibleGroup(){
  const out = [];
  for (let i = 0; i < CARDS && ITEMS.length; i++) out.push(ITEMS[(idx + i) % ITEMS.length]);
  return out;
}
// Llegeix el grup visible (els que tenen veu; els que no, els saltem). No es
// torna a llegir el mateix grup seguit. Els ítems sense veu encara es tornen
// a provar aviat (l'MP3 es genera mentre s'analitza la cua).
function maybeReadGroup(force){
  if (reading) return; // no interrompem el grup en curs
  const group = visibleGroup();
  const key = group.map(l => l.id).join(',');
  if (!force && key === lastGroupKey) return;
  lastGroupKey = key;
  const urls = group.filter(l => l.audio).map(l => l.audio);
  missingIds = group.filter(l => !l.audio).map(l => l.id);
  if (urls.length){ audioQueue = urls.slice(); playQueue(); }
  scheduleMissingRetry();
}
let missingTimer = null;
function scheduleMissingRetry(){
  if (missingTimer) clearTimeout(missingTimer);
  if (!missingIds.length) return;
  missingTimer = setTimeout(() => {
    missingTimer = null;
    if (reading) return;
    // Reintenta els ítems d'aquest grup que encara no tenien veu: potser el
    // MP3 s'ha generat mentre re-carregàvem el ticker.
    const ready = missingIds
      .map(id => ITEMS.find(x => x.id === id))
      .filter(l => l && l.audio);
    missingIds = [];
    if (ready.length){
      audioQueue = ready.map(l => l.audio).slice();
      playQueue();
    }
  }, 20000);
}

// ---- Crawl inferior ----
function renderCrawl() {
  if (!ITEMS.length) return;
  // Doble del contingut per fer la transició de bucle contínua (-50%).
  const twice = ITEMS.concat(ITEMS).map(l =>
    `<span class="crawl-item"><span class="csrc">${esc(l.source)}</span>` +
    `<span class="ctime">${esc(fmtDate(l.date))}</span>` +
    (isNew(l) ? '<span class="cnew">● NOU</span>' : '') +
    `<span>${esc(l.title)}</span><span class="cdot">•</span></span>`).join('');
  const t = $('crawl');
  t.innerHTML = twice;
  // Durada proporcional: ~1.6s per ítem (16.5px * ~0.9 caràcters...). Aproximadament lineal.
  const dur = Math.max(40, ITEMS.length * 3.2);
  t.style.animationDuration = dur + 's';
}

// ---- Dades ----
let musicStarted = false;
async function load() {
  try {
    const r = await fetch('/overlay/ticker.json?v=' + DATAV, { cache:'no-store' });
    const j = await r.json();
    if (j && Array.isArray(j.items) && j.items.length) {
      const same = ITEMS.length && ITEMS[0].id === j.items[0].id;
      ITEMS = j.items;
      if (idx >= ITEMS.length) idx = 0;
      renderCards();
      maybeReadGroup();
      if (!same) renderCrawl();
      if (!musicStarted) { musicStarted = true; startMusic(); }
    }
  } catch (e) { console.warn('ticker', e); }
}
load();
setInterval(load, REFRESH);
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_prose_turns_markdown_into_plain_prose() {
        let s = "- Hola\n- **Món**\n\n> cita descartada\n# Títol amb # dins\nText final";
        let out = clean_prose(s);
        assert!(!out.contains('*'), "no hi ha asteriscos sobrants: {out:?}");
        assert!(!out.contains('>'), "les cites es descarten");
        assert!(out.contains("Hola"));
        assert!(out.contains("Món"));
        assert!(out.contains("Títol amb"));
        assert!(!out.contains("cita descartada"));
        assert!(out.contains("Text final"));
        // Un sol paràgraf continu.
        assert!(!out.contains('\n'));
    }

    #[test]
    fn clean_prose_ignores_empty() {
        assert_eq!(clean_prose("    \n\n  "), "");
        assert_eq!(clean_prose(""), "");
    }

    /// Guarda: l'escena mostra més text (clamp configurable) i la rotació salta
    /// de CARDS en CARDS (tota la graella canvia), i la imatge cedeix espai al text.
    #[test]
    fn overlay_template_layout_and_rotation_contract() {
        assert!(OVERLAY_HTML.contains("-webkit-line-clamp:{{LINES}}"));
        assert!(OVERLAY_HTML.contains("idx = (idx + CARDS) % ITEMS.length"));
        assert!(OVERLAY_HTML.contains(".sum"),
                "el cos de la card fa servir .sum");
        assert!(OVERLAY_HTML.contains("height:clamp(110px, 17vh, 180px)"),
                "la imatge té alçada limitada per donar espai al text");
        // La card mostra l'anàlisi (text) amb fallback al resum.
        assert!(OVERLAY_HTML.contains("l.text && l.text.trim()"));
        // La card i el crawl mostren dia/hora de la notícia i el tipus en català.
        assert!(OVERLAY_HTML.contains("fmtDate(l.date)"),
                "la card/crawl renderitzen la data de la notícia");
        assert!(OVERLAY_HTML.contains("typeLabel(l.link_type)"),
                "la card mostra el tipus de notícia");
        assert!(OVERLAY_HTML.contains("Notícia"),
                "hi ha etiquetes de tipus en català");
        // TimeZone configurable per a les hores de l'escena.
        assert!(OVERLAY_HTML.contains("TZ_OPT"),
                "les hores respecten la TimeZone configurada");
        assert!(OVERLAY_HTML.contains("{{TIMEZONE}}"),
                "la TimeZone s'injecta des de config");
        // Notícies recents (finestra configurable) marcades com a NOVA amb marc/cintó.
        assert!(OVERLAY_HTML.contains("NEW_MS"),
                "hi ha una finestra de 'nova'");
        assert!(OVERLAY_HTML.contains("{{NEW_MIN}}"),
                "la finestra de 'nova' s'injecta des de config (minuts)");
        assert!(OVERLAY_HTML.contains("Math.max(0, {{NEW_MIN}})"),
                "0 = marca de nova desactivada");
        assert!(OVERLAY_HTML.contains("isNew(l)"),
                "card i crawl marquen la notícia nova");
        assert!(OVERLAY_HTML.contains("badge-new") && OVERLAY_HTML.contains("card.new"),
                "la notícia nova té marc i etiqueta NOU");
        // Veu dels titulars + música de fons: la core llegeix el grup visible
        // i la música fa ducking mentre parla.
        assert!(OVERLAY_HTML.contains("maybeReadGroup()"),
                "la rotació re-llança la lectura del grup");
        assert!(OVERLAY_HTML.contains("/music.mp3"),
                "la música de fons ve de public/music.mp3");
        assert!(OVERLAY_HTML.contains("MUSIC_DUCK") && OVERLAY_HTML.contains("duckForSpeech"),
                "ducking de la música mentre es llegeixen titulars");
        assert!(OVERLAY_HTML.contains("{{MUSIC_VOL}}")
                && OVERLAY_HTML.contains("{{MUSIC_DUCK}}")
                && OVERLAY_HTML.contains("{{READ_GAP}}"),
                "els paràmetres de veu/música s'injecten des de config");
    }

    /// Cada element del ticker porta la data/hora (RFC3339) de la notícia.
    #[test]
    fn ticker_item_includes_date() {
        use crate::models::{DeepStatus, LinkStatus, LinkType, Sentiment};
        let link = Link {
            id: uuid::Uuid::new_v4(),
            url: "https://example.com/noticia".into(),
            title: Some("títol".into()),
            summary: Some("resum".into()),
            link_type: LinkType::Article,
            tags: vec![],
            sentiment: Sentiment::Neutral,
            status: LinkStatus::Done,
            co_reporters: vec![],
            reporters: vec!["npc1".into()],
            deep_status: DeepStatus::Done,
            deep_summary: Some("anàlisi **profunda**".into()),
            code_stats: None,
            image_url: None,
            image_file: None,
            audio_file: Some("t.mp3".into()),
            embedding: None,
            embed_scale: None,
            created_at: chrono::DateTime::parse_from_rfc3339("2025-08-27T09:48:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            updated_at: chrono::Utc::now(),
        };
        let v = item(&link);
        assert_eq!(v["date"], "2025-08-27T09:48:00+00:00");
        assert_eq!(v["link_type"], "article");
        // Amb audio_file, la card porta la URL del MP3 de la veu.
        assert_eq!(v["audio"], format!("/audio/{}", link.id));
    }

    /// Sense audio_file, la card no ofereix veu (camp buit).
    #[test]
    fn ticker_item_audio_null_without_file() {
        use crate::models::{DeepStatus, LinkStatus, LinkType, Sentiment};
        let link = Link {
            id: uuid::Uuid::new_v4(),
            url: "https://example.com/x".into(),
            title: Some("títol".into()),
            summary: None,
            link_type: LinkType::News,
            tags: vec![],
            sentiment: Sentiment::Neutral,
            status: LinkStatus::Done,
            co_reporters: vec![],
            reporters: vec![],
            deep_status: DeepStatus::Done,
            deep_summary: Some("anàlisi".into()),
            code_stats: None,
            image_url: None,
            image_file: None,
            audio_file: None,
            embedding: None,
            embed_scale: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let v = item(&link);
        assert!(v["audio"].is_null(), "sense fitxer no hi ha veu");
    }

    /// `overlay_html` injecta la TimeZone i la finestra de "nova" al JS.
    #[test]
    fn overlay_html_injects_config_constants() {
        std::env::remove_var("OVERLAY_TIMEZONE");
        std::env::remove_var("OVERLAY_NEW_MINUTES");
        let html = overlay_html(&Config::from_env().unwrap());
        assert!(
            html.contains("const TZ = \"\";"),
            "sense TimeZone la constant queda buida"
        );
        assert!(
            html.contains("const NEW_MIN = Math.max(0, 30);"),
            "per defecte la finestra de nova és 30 minuts"
        );
        assert!(
            html.contains("const MUSIC_VOL  = parseFloat(0.22);"),
            "volum de música per defecte 0.22"
        );
        assert!(
            html.contains("const MUSIC_DUCK = parseFloat(0.07);"),
            "ducking per defecte 0.07"
        );
        assert!(
            html.contains("const READ_GAP   = Math.max(0, parseInt(500, 10) || 400);"),
            "pausa entre titulars per defecte 500 ms"
        );

        std::env::set_var("OVERLAY_TIMEZONE", "Europe/Andorra");
        std::env::set_var("OVERLAY_NEW_MINUTES", "15");
        let html = overlay_html(&Config::from_env().unwrap());
        assert!(
            html.contains("const TZ = \"Europe/Andorra\";"),
            "la TimeZone surt JSON-quoted al JS"
        );
        assert!(
            html.contains("const NEW_MIN = Math.max(0, 15);"),
            "la finestra de nova es fa configurable (15 minuts)"
        );
        std::env::remove_var("OVERLAY_TIMEZONE");
        std::env::remove_var("OVERLAY_NEW_MINUTES");
    }

    /// Percent-encoding per al proxy: els caràcters reservats van a %XX.
    #[test]
    fn percent_encode_reserved() {
        assert_eq!(percent_encode("https://x.com/a b?c=d&e=f"), "https%3A%2F%2Fx.com%2Fa%20b%3Fc%3Dd%26e%3Df");
    }

    /// La card prefereix la còpia LOCAL (/imgout/{id}) i cau al proxy remot si
    /// no hi ha imatge local.
    #[test]
    fn image_prefers_local_copy() {
        use crate::models::{DeepStatus, LinkStatus, LinkType, Sentiment};
        let mk = |image_url: Option<&str>, image_file: Option<&str>| Link {
            id: uuid::Uuid::new_v4(),
            url: "https://example.com/noticia".into(),
            title: Some("títol".into()),
            summary: Some("resum".into()),
            link_type: LinkType::Article,
            tags: vec![],
            sentiment: Sentiment::Neutral,
            status: LinkStatus::Done,
            co_reporters: vec![],
            reporters: vec![],
            deep_status: DeepStatus::Done,
            deep_summary: Some("anàlisi".into()),
            code_stats: None,
            image_url: image_url.map(String::from),
            image_file: image_file.map(String::from),
            audio_file: None,
            embedding: None,
            embed_scale: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let with_local = mk(Some("https://cdn/x.jpg"), Some("abc.jpg"));
        assert_eq!(
            image_for(&with_local),
            Some(format!("/imgout/{}", with_local.id))
        );
        let only_remote = mk(Some("https://cdn/x.jpg"), None);
        assert_eq!(
            image_for(&only_remote),
            Some("/img?u=https%3A%2F%2Fcdn%2Fx.jpg".into())
        );
        assert_eq!(image_for(&mk(None, None)), None);
    }
}
