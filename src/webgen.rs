use crate::config::Config;
use crate::db::Db;
use crate::error::{AppError, Result};
use std::path::Path;
use std::process::Command;

/// Genera la web estatica a cfg.public_dir.
pub async fn generate(db: &Db, cfg: &Config) -> Result<()> {
    let links = db.list_links(None, None, None, 5000).await?;
    let dir = Path::new(&cfg.public_dir);
    std::fs::create_dir_all(dir.join("data"))?;
    std::fs::create_dir_all(dir.join("css"))?;
    std::fs::create_dir_all(dir.join("js"))?;

    let json = serde_json::to_string(&links)?;
    let json_pretty = serde_json::to_string_pretty(&links)?;
    // links.json per consum extern; links.js incrustat perquè funcioni via file://
    // (fetch() està bloquejat sota file:// a la majoria de navegadors).
    std::fs::write(dir.join("data/links.json"), json_pretty)?;
    std::fs::write(
        dir.join("data/links.js"),
        format!("window.__LINKS__ = {json};\n"),
    )?;
    std::fs::write(dir.join("index.html"), INDEX_HTML)?;
    std::fs::write(dir.join("css/style.css"), STYLE_CSS)?;
    std::fs::write(dir.join("js/app.js"), APP_JS)?;

    tracing::info!(count = links.len(), dir = %cfg.public_dir, "static web generated");
    Ok(())
}

/// Commit + push opt-in. Nomes si cfg.git.push_enabled().
pub fn git_push(cfg: &Config, message: &str) -> Result<()> {
    if !cfg.git.push_enabled() {
        tracing::info!("git push skipped (WEB_REPO_URL not set)");
        return Ok(());
    }
    let dir = &cfg.public_dir;
    let branch = &cfg.git.web_branch;

    // Init repo si cal.
    if !Path::new(dir).join(".git").exists() {
        run_git(dir, &["init"])?;
        run_git(dir, &["checkout", "-B", branch])?;
    }

    // Configura remote amb token si s'ha donat.
    let remote_url = match (&cfg.git.web_repo_url, &cfg.git.git_token) {
        (Some(url), Some(token)) => inject_token(url, token),
        (Some(url), None) => url.clone(),
        _ => return Err(AppError::Git("no web_repo_url".into())),
    };
    // set-url falla si no existeix => prova add.
    if run_git(dir, &["remote", "set-url", "origin", &remote_url]).is_err() {
        run_git(dir, &["remote", "add", "origin", &remote_url])?;
    }

    run_git(dir, &["add", "."])?;
    // commit pot fallar si no hi ha canvis: ho tolerem.
    let _ = run_git(dir, &["commit", "-m", message]);
    run_git(dir, &["push", "-u", "origin", branch])?;
    tracing::info!("static web pushed to git");
    Ok(())
}

fn inject_token(url: &str, token: &str) -> String {
    // https://github.com/u/r.git -> https://<token>@github.com/u/r.git
    if let Some(rest) = url.strip_prefix("https://") {
        format!("https://{token}@{rest}")
    } else {
        url.to_string()
    }
}

fn run_git(dir: &str, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| AppError::Git(format!("spawn git: {e}")))?;
    if !out.status.success() {
        return Err(AppError::Git(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="ca" data-theme="dark">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Clio · LinkAnalyzer</title>
  <meta name="description" content="Enllaços recollits, analitzats i resumits per Clio.">
  <link rel="stylesheet" href="css/style.css">
</head>
<body>
  <header class="topbar">
    <div class="topbar-inner">
      <div class="brand">
        <span class="brand-mark">◆</span>
        <div>
          <h1>Clio</h1>
          <p class="tagline">Enllaços analitzats &amp; resumits</p>
        </div>
      </div>
      <div class="topbar-actions">
        <button id="admin-btn" class="theme-toggle" aria-label="Usuaris" title="Gestió d'usuaris" style="display:none">👤</button>
        <button id="token-btn" class="theme-toggle" aria-label="Sessió" title="Introdueix el teu API token">🔑</button>
        <button id="theme-toggle" class="theme-toggle" aria-label="Canvia el tema" title="Canvia el tema">
          <span class="theme-icon"></span>
        </button>
      </div>
    </div>
    <div class="topbar-inner controls">
      <div class="search-wrap">
        <svg class="search-ico" viewBox="0 0 24 24" width="18" height="18" aria-hidden="true"><path fill="currentColor" d="M21 20l-5.6-5.6a7 7 0 10-1.4 1.4L20 21zM5 10a5 5 0 1110 0 5 5 0 01-10 0z"/></svg>
        <input id="search" type="search" placeholder="Cerca per títol o resum…" autocomplete="off">
      </div>
      <select id="type-filter" class="select" aria-label="Filtra per tipus">
        <option value="">Tots els tipus</option>
        <option value="news">News</option>
        <option value="repo">Repo</option>
        <option value="article">Article</option>
        <option value="video">Video</option>
        <option value="blog">Blog</option>
        <option value="other">Other</option>
      </select>
      <select id="sent-filter" class="select" aria-label="Filtra per sentiment">
        <option value="">Tot sentiment</option>
        <option value="positive">Positiu</option>
        <option value="neutral">Neutral</option>
        <option value="negative">Negatiu</option>
      </select>
    </div>
    <div id="stats" class="stats"></div>
    <div id="perso" class="perso"></div>
    <div id="filters" class="filters"></div>
  </header>
  <main id="grid" class="grid"></main>
  <footer class="footer"><p>Generat per <strong>Clio</strong> · LinkAnalyzer</p></footer>
  <script src="data/links.js"></script>
  <script src="js/app.js"></script>
</body>
</html>
"#;

const STYLE_CSS: &str = r#":root {
  --radius: 14px;
  --maxw: 1180px;
  --shadow: 0 1px 2px rgba(0,0,0,.06), 0 6px 24px rgba(0,0,0,.08);
  --transition: .18s ease;
}
html[data-theme="dark"] {
  --bg: #0d1017; --bg-soft: #121722; --card: #161c28; --card-hover: #1b2230;
  --fg: #e8ecf3; --muted: #93a0b5; --faint: #5c6679;
  --border: #232c3b; --accent: #6aa8ff; --accent-ink: #06101f;
  --pos: #45d49a; --neu: #8aa0b8; --neg: #ff6b7a;
  --shadow: 0 1px 2px rgba(0,0,0,.4), 0 10px 30px rgba(0,0,0,.35);
}
html[data-theme="light"] {
  --bg: #f5f7fb; --bg-soft: #eef1f7; --card: #ffffff; --card-hover: #fbfcfe;
  --fg: #16202e; --muted: #5a6678; --faint: #8a96a8;
  --border: #e2e7f0; --accent: #2f6bff; --accent-ink: #ffffff;
  --pos: #15a06a; --neu: #6a7689; --neg: #d23b4d;
}
* { box-sizing: border-box; }
html, body { margin: 0; }
body {
  font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
  background: var(--bg); color: var(--fg);
  -webkit-font-smoothing: antialiased;
  transition: background var(--transition), color var(--transition);
}

/* ---- Topbar ---- */
.topbar {
  position: sticky; top: 0; z-index: 10;
  background: color-mix(in srgb, var(--bg) 88%, transparent);
  backdrop-filter: saturate(140%) blur(12px);
  border-bottom: 1px solid var(--border);
  padding: 1rem 1.25rem .85rem;
}
.topbar-inner { max-width: var(--maxw); margin: 0 auto; display: flex; align-items: center; justify-content: space-between; gap: 1rem; }
.brand { display: flex; align-items: center; gap: .7rem; }
.brand-mark {
  display: grid; place-items: center; width: 40px; height: 40px; border-radius: 11px;
  background: linear-gradient(135deg, var(--accent), color-mix(in srgb, var(--accent) 55%, #b06aff));
  color: #fff; font-size: 1.1rem; box-shadow: var(--shadow);
}
.brand h1 { margin: 0; font-size: 1.25rem; letter-spacing: -.02em; }
.tagline { margin: 0; font-size: .78rem; color: var(--muted); }

.theme-toggle {
  width: 40px; height: 40px; border-radius: 11px; cursor: pointer;
  border: 1px solid var(--border); background: var(--card); color: var(--fg);
  display: grid; place-items: center; transition: all var(--transition);
}
.theme-toggle:hover { background: var(--card-hover); transform: translateY(-1px); }
.theme-icon::before { content: "🌙"; font-size: 1.1rem; }
html[data-theme="light"] .theme-icon::before { content: "☀️"; }

.controls { margin-top: .85rem; flex-wrap: wrap; }
.search-wrap { position: relative; flex: 1 1 280px; min-width: 220px; }
.search-ico { position: absolute; left: .7rem; top: 50%; transform: translateY(-50%); color: var(--muted); pointer-events: none; }
#search {
  width: 100%; padding: .6rem .9rem .6rem 2.3rem; font-size: .95rem;
  border-radius: 10px; border: 1px solid var(--border); background: var(--card); color: var(--fg);
  transition: border-color var(--transition), box-shadow var(--transition);
}
#search:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 25%, transparent); }
.select {
  padding: .6rem .8rem; border-radius: 10px; border: 1px solid var(--border);
  background: var(--card); color: var(--fg); font-size: .9rem; cursor: pointer;
}
.select:focus { outline: none; border-color: var(--accent); }

.stats { max-width: var(--maxw); margin: .85rem auto 0; display: flex; gap: 1.2rem; font-size: .82rem; color: var(--muted); }
.stats b { color: var(--fg); }

.filters { max-width: var(--maxw); margin: .7rem auto 0; display: flex; flex-wrap: wrap; gap: .4rem; }

/* ---- Chips ---- */
.chip {
  cursor: pointer; font-size: .76rem; padding: .22rem .6rem; border-radius: 999px;
  background: var(--bg-soft); color: var(--muted); border: 1px solid var(--border);
  transition: all var(--transition); user-select: none;
}
.chip:hover { color: var(--fg); border-color: var(--accent); }
.chip.active { background: var(--accent); color: var(--accent-ink); border-color: var(--accent); }
.tags .chip { cursor: pointer; }

/* ---- Grid + cards ---- */
.grid {
  max-width: var(--maxw); margin: 0 auto; padding: 1.4rem 1.25rem 2rem;
  display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 1.1rem;
}
.card {
  position: relative; background: var(--card); border: 1px solid var(--border);
  border-radius: var(--radius); padding: 1.1rem 1.1rem 1rem;
  display: flex; flex-direction: column; gap: .6rem;
  transition: transform var(--transition), border-color var(--transition), box-shadow var(--transition);
}
.card:hover { transform: translateY(-3px); border-color: color-mix(in srgb, var(--accent) 45%, var(--border)); box-shadow: var(--shadow); }
.card-top { display: flex; align-items: center; justify-content: space-between; gap: .5rem; }
.card h2 { font-size: 1.04rem; line-height: 1.3; margin: 0; letter-spacing: -.01em;
  display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }
.card h2 a { color: var(--fg); text-decoration: none; }
.card h2 a:hover { color: var(--accent); }

.badge {
  flex: none; font-size: .66rem; font-weight: 600; text-transform: uppercase; letter-spacing: .05em;
  padding: .2rem .5rem; border-radius: 6px; border: 1px solid transparent;
}
.badge.t-news    { color: #ff9f43; background: color-mix(in srgb, #ff9f43 16%, transparent); }
.badge.t-repo    { color: #8b7dff; background: color-mix(in srgb, #8b7dff 16%, transparent); }
.badge.t-article { color: var(--accent); background: color-mix(in srgb, var(--accent) 16%, transparent); }
.badge.t-video   { color: #ff6b7a; background: color-mix(in srgb, #ff6b7a 16%, transparent); }
.badge.t-blog    { color: #45d49a; background: color-mix(in srgb, #45d49a 16%, transparent); }
.badge.t-other   { color: var(--muted); background: var(--bg-soft); }

.summary { color: var(--muted); font-size: .9rem; line-height: 1.5; margin: 0;
  display: -webkit-box; -webkit-line-clamp: 4; -webkit-box-orient: vertical; overflow: hidden; }
.tags { display: flex; flex-wrap: wrap; gap: .3rem; }
.meta { display: flex; align-items: center; justify-content: space-between; font-size: .8rem; color: var(--muted); margin-top: auto; padding-top: .3rem; border-top: 1px solid var(--border); }
.sent { display: inline-flex; align-items: center; gap: .35rem; font-weight: 500; }
.dot { width: 8px; height: 8px; border-radius: 50%; }
.sent.positive { color: var(--pos); } .sent.positive .dot { background: var(--pos); }
.sent.neutral  { color: var(--neu); } .sent.neutral  .dot { background: var(--neu); }
.sent.negative { color: var(--neg); } .sent.negative .dot { background: var(--neg); }

/* ---- Deep pass ---- */
.codestats { display: flex; flex-wrap: wrap; align-items: center; gap: .6rem; font-size: .76rem; color: var(--muted); }
.codestats .langs { display: flex; flex-wrap: wrap; gap: .35rem; }
.codestats .lang { background: var(--bg-soft); border: 1px solid var(--border); border-radius: 6px; padding: .1rem .4rem; }
.codestats .lang i { color: var(--accent); font-style: normal; }
.deep { border-top: 1px dashed var(--border); padding-top: .5rem; font-size: .85rem; }
.deep summary { cursor: pointer; color: var(--accent); font-weight: 500; user-select: none; }
.deep p { color: var(--muted); line-height: 1.5; margin: .5rem 0 0; }
.deep-pending { font-size: .76rem; color: var(--faint); font-style: italic; }

/* Markdown de l'anàlisi profunda */
.deep-md { color: var(--muted); line-height: 1.55; margin-top: .5rem; }
.deep-md h1, .deep-md h2, .deep-md h3, .deep-md h4 { color: var(--fg); margin: .8rem 0 .35rem; line-height: 1.3; }
.deep-md h1 { font-size: 1.02rem; } .deep-md h2 { font-size: .96rem; } .deep-md h3, .deep-md h4 { font-size: .9rem; }
.deep-md p { margin: .5rem 0; }
.deep-md ul { margin: .4rem 0; padding-left: 1.2rem; }
.deep-md li { margin: .15rem 0; }
.deep-md a { color: var(--accent); }
.deep-md code { background: var(--bg-soft); border: 1px solid var(--border); border-radius: 5px; padding: .05rem .3rem; font-size: .85em; }
.deep-md pre { background: var(--bg-soft); border: 1px solid var(--border); border-radius: 8px; padding: .6rem .8rem; overflow-x: auto; }
.deep-md pre code { background: none; border: none; padding: 0; }
.deep-md strong { color: var(--fg); }

/* Reporters (qui ha enviat l'enllaç) */
.reporters { display: flex; flex-wrap: wrap; gap: .25rem; justify-content: flex-end; }
.chip.user { font-size: .72rem; padding: .12rem .45rem; }
.chip.user:hover { color: var(--accent); border-color: var(--accent); }

/* Novetats */
.badge-new {
  flex: none; font-size: .62rem; font-weight: 700; letter-spacing: .06em;
  padding: .18rem .42rem; border-radius: 6px;
  color: var(--accent-ink); background: var(--accent);
}
.card.is-new { border-color: color-mix(in srgb, var(--accent) 55%, var(--border)); }
.card.is-new::before {
  content: ""; position: absolute; inset: 0; border-radius: var(--radius);
  box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--accent) 40%, transparent); pointer-events: none;
}
.new-toggle { cursor: pointer; user-select: none; transition: color var(--transition); }
.new-toggle:hover { color: var(--fg); }
.new-toggle.on { color: var(--accent); }
.new-toggle.on b { color: var(--accent); }

/* Sessió + accions per link */
.topbar-actions { display: flex; gap: .5rem; }
#token-btn.on { border-color: var(--accent); color: var(--accent); }
.actions { display: flex; gap: .4rem; margin-top: .2rem; }
.act {
  cursor: pointer; font-size: .76rem; padding: .28rem .6rem; border-radius: 8px;
  border: 1px solid var(--border); background: var(--bg-soft); color: var(--muted);
  transition: all var(--transition);
}
.act:hover { color: var(--fg); border-color: var(--accent); }
.act-delete:hover { color: var(--neg); border-color: var(--neg); }

/* Toast */
.toast {
  position: fixed; left: 50%; bottom: 1.2rem; transform: translate(-50%, 2rem);
  background: var(--card); color: var(--fg); border: 1px solid var(--border);
  border-radius: 10px; padding: .6rem 1rem; font-size: .88rem; box-shadow: var(--shadow);
  opacity: 0; pointer-events: none; transition: all var(--transition); z-index: 50; max-width: 90vw;
}
.toast.show { opacity: 1; transform: translate(-50%, 0); }
.toast.ok { border-color: var(--pos); }
.toast.err { border-color: var(--neg); }

/* Modal d'usuaris */
.modal-ov {
  position: fixed; inset: 0; z-index: 100; display: grid; place-items: center;
  background: rgba(0,0,0,.5); backdrop-filter: blur(3px); padding: 1rem;
}
.modal {
  width: min(640px, 100%); max-height: 86vh; overflow: auto;
  background: var(--card); border: 1px solid var(--border); border-radius: var(--radius);
  box-shadow: var(--shadow);
}
.modal-head { display: flex; align-items: center; justify-content: space-between;
  padding: .9rem 1.1rem; border-bottom: 1px solid var(--border); position: sticky; top: 0; background: var(--card); }
.modal-head h3 { margin: 0; font-size: 1.05rem; }
.modal-x { cursor: pointer; background: none; border: none; color: var(--muted); font-size: 1.1rem; }
.modal-x:hover { color: var(--fg); }
.modal-body { padding: 1.1rem; }
.utable { width: 100%; border-collapse: collapse; font-size: .88rem; }
.utable th { text-align: left; color: var(--faint); font-weight: 500; font-size: .76rem; text-transform: uppercase; letter-spacing: .04em; padding: .3rem .4rem; }
.utable td { padding: .45rem .4rem; border-top: 1px solid var(--border); vertical-align: middle; }
.utable .you { color: var(--faint); font-size: .78rem; }
.urow-actions { display: flex; gap: .3rem; justify-content: flex-end; flex-wrap: wrap; }
.urow-actions .act { padding: .2rem .45rem; }
.act[disabled] { opacity: .35; cursor: not-allowed; }
.rolebadge { font-size: .72rem; padding: .1rem .45rem; border-radius: 6px; border: 1px solid var(--border); }
.rolebadge.admin { color: var(--accent); border-color: color-mix(in srgb, var(--accent) 50%, var(--border)); }
.rolebadge.user { color: var(--muted); }
.ucreate { display: flex; gap: .5rem; align-items: center; margin-top: 1rem; padding-top: 1rem; border-top: 1px dashed var(--border); flex-wrap: wrap; }
.ucreate input[type=text], .ucreate #nu-name {
  flex: 1 1 200px; padding: .45rem .7rem; border-radius: 8px;
  border: 1px solid var(--border); background: var(--bg-soft); color: var(--fg); font-size: .88rem;
}
.nu-adm { display: inline-flex; align-items: center; gap: .3rem; color: var(--muted); font-size: .85rem; }

/* ---- Cor / personalització ---- */
.card-top-right { display: flex; align-items: center; gap: .45rem; flex: none; }
.heart {
  cursor: pointer; background: none; border: none; padding: .05rem .15rem;
  font-size: 1.1rem; line-height: 1; color: var(--faint);
  transition: color var(--transition), transform var(--transition);
}
.heart:hover { color: var(--neg); transform: scale(1.18); }
.heart.on { color: var(--neg); }
.perso { max-width: var(--maxw); margin: .7rem auto 0; display: none; align-items: center; gap: .8rem; font-size: .82rem; color: var(--muted); }
.perso.on { display: flex; }
.perso b { color: var(--fg); }
.perso-clear {
  cursor: pointer; font-size: .76rem; padding: .2rem .6rem; border-radius: 999px;
  border: 1px solid var(--border); background: var(--bg-soft); color: var(--muted);
  transition: all var(--transition);
}
.perso-clear:hover { color: var(--fg); border-color: var(--accent); }

.empty { grid-column: 1/-1; text-align: center; color: var(--muted); padding: 3rem 1rem; font-size: .95rem; }

.footer { text-align: center; color: var(--muted); padding: 2rem; font-size: .82rem; border-top: 1px solid var(--border); }

@media (max-width: 560px) {
  .grid { grid-template-columns: 1fr; }
  .tagline { display: none; }
}
"#;

const APP_JS: &str = r#""use strict";

// Dades: incrustades a data/links.js (window.__LINKS__) per funcionar via file://.
// Fallback a fetch si s'està servint per HTTP i no hi ha incrustat.
let ALL = Array.isArray(window.__LINKS__) ? window.__LINKS__ : [];
let activeTag = null;   // filtre per tag (#tag:xxx)
let activeUser = null;  // filtre per reporter (#at:xxx)
let onlyNew = false;    // mostra només novetats (links no vistos)

const $ = (id) => document.getElementById(id);

// ---- Novetats: marca de temps de l'última visita (cookie) ----
// Es captura a l'arrencada (abans de rerenderitzar) per poder ressaltar els
// links creats després; després s'actualitza a "ara".
function readLastVisit() {
  const m = document.cookie.match(/(?:^|;\s*)clio_seen=([^;]*)/);
  return m ? (parseInt(decodeURIComponent(m[1]), 10) || 0) : 0;
}
function writeLastVisit(ms) {
  document.cookie = 'clio_seen=' + ms + '; path=/; max-age=31536000; SameSite=Lax';
}
const LAST_VISIT = readLastVisit();
function linkTime(l) { const t = Date.parse(l.created_at); return isNaN(t) ? 0 : t; }
function isNew(l) { return LAST_VISIT > 0 && linkTime(l) > LAST_VISIT; }

// ---- Sessió / API ----
// Les accions (clau, refer, baixa, usuaris) només tenen sentit contra un
// servei viu (mode `serve`). Es detecta amb /api/v1/ping: la web estàtica pura
// (file:// o hosting sense backend) no respon i s'amaga tota la UI d'accions.
let API_LIVE = false;       // determinat per probeApi()
let ME = null;              // {id, username, role} de /api/v1/me
function getToken() { return localStorage.getItem('clio-token') || ''; }
function setToken(t) { if (t) localStorage.setItem('clio-token', t); else localStorage.removeItem('clio-token'); }
function hasToken() { return API_LIVE && !!getToken(); }
function isAdmin() { return API_LIVE && ME && ME.role === 'admin'; }

async function probeApi() {
  try { const r = await fetch('/api/v1/ping', { cache: 'no-store' }); API_LIVE = r.ok; }
  catch (e) { API_LIVE = false; }
}

async function api(method, path, body) {
  const opts = { method, headers: { 'Authorization': 'Bearer ' + getToken() } };
  if (body !== undefined) { opts.headers['Content-Type'] = 'application/json'; opts.body = JSON.stringify(body); }
  const r = await fetch('/api/v1' + path, opts);
  if (!r.ok) {
    const j = await r.json().catch(() => ({}));
    throw new Error(j.error || ('HTTP ' + r.status));
  }
  return r.json().catch(() => ({}));
}

async function loadMe() {
  ME = null;
  if (!API_LIVE || !getToken()) return;
  try { ME = await api('GET', '/me'); } catch (e) { ME = null; }
}

// Toast efímer a baix de la pantalla.
let toastTimer = null;
function toast(msg, kind) {
  let el = $('toast');
  if (!el) { el = document.createElement('div'); el.id = 'toast'; document.body.appendChild(el); }
  el.textContent = msg;
  el.className = 'toast show' + (kind ? ' ' + kind : '');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.className = 'toast'; }, 3200);
}

async function reprocessLink(id) {
  try { await api('POST', '/links/' + id + '/reprocess'); toast('Link reencuat: es tornarà a analitzar.', 'ok'); }
  catch (e) { toast('Error en reforçar: ' + e.message, 'err'); }
}
async function deleteLink(id) {
  if (!confirm('Segur que vols donar de baixa aquest link?')) return;
  try {
    await api('DELETE', '/links/' + id);
    ALL = ALL.filter(l => l.id !== id);
    hearts.delete(id);
    renderStats(); buildFilters(); render();
    toast('Link donat de baixa.', 'ok');
  } catch (e) { toast('Error en donar de baixa: ' + e.message, 'err'); }
}

function initTokenButton() {
  const btn = $('token-btn');
  if (!btn) return;
  if (!API_LIVE) { btn.style.display = 'none'; return; }
  const refresh = () => { btn.classList.toggle('on', !!getToken()); btn.title = getToken() ? 'Sessió activa · clica per canviar/treure el token' : 'Introdueix el teu API token'; };
  refresh();
  btn.onclick = async () => {
    const cur = getToken();
    const t = prompt(cur ? 'API token (buit per tancar sessió):' : 'Enganxa el teu API token:', cur);
    if (t === null) return;
    setToken(t.trim());
    await loadMe();
    refresh(); refreshAdminBtn(); render();
    toast(getToken() ? 'Sessió iniciada.' : 'Sessió tancada.', 'ok');
  };
}

// ---- Admin: gestió d'usuaris ----
function refreshAdminBtn() {
  const b = $('admin-btn');
  if (!b) return;
  b.style.display = isAdmin() ? '' : 'none';
  b.onclick = openUsersModal;
}

// Mostra un token un sol cop (és copiable des del prompt).
function showToken(username, token) {
  prompt("Token de " + username + " (copia'l ara, no es tornarà a mostrar):", token);
}

async function openUsersModal() {
  let users;
  try { users = (await api('GET', '/users')).users || []; }
  catch (e) { toast('Error carregant usuaris: ' + e.message, 'err'); return; }

  const ov = document.createElement('div');
  ov.className = 'modal-ov';
  ov.innerHTML = `<div class="modal">
    <div class="modal-head"><h3>👤 Usuaris</h3><button class="modal-x" title="Tanca">✕</button></div>
    <div class="modal-body">
      <table class="utable">
        <thead><tr><th>Usuari</th><th>Rol</th><th>Creat</th><th></th></tr></thead>
        <tbody id="ulist"></tbody>
      </table>
      <div class="ucreate">
        <input id="nu-name" placeholder="nom del nou usuari" autocomplete="off">
        <label class="nu-adm"><input type="checkbox" id="nu-admin"> admin</label>
        <button id="nu-add" class="act">+ Crea</button>
      </div>
    </div>
  </div>`;
  document.body.appendChild(ov);
  const close = () => ov.remove();
  ov.querySelector('.modal-x').onclick = close;
  ov.onclick = (e) => { if (e.target === ov) close(); };
  document.addEventListener('keydown', function esc2(e){ if(e.key==='Escape'){ close(); document.removeEventListener('keydown', esc2);} });

  const refresh = async () => {
    try { fill((await api('GET', '/users')).users || []); }
    catch (e) { toast(e.message, 'err'); }
  };
  function fill(list) {
    const tb = ov.querySelector('#ulist');
    tb.innerHTML = '';
    list.forEach(u => {
      const tr = document.createElement('tr');
      const mine = ME && u.id === ME.id;
      tr.innerHTML = `<td>${esc(u.username)}${mine ? ' <span class="you">(tu)</span>' : ''}</td>
        <td><span class="rolebadge ${u.role}">${u.role}</span></td>
        <td>${(u.created_at || '').slice(0,10)}</td>
        <td class="urow-actions">
          <button class="act" data-act="role" title="Canvia el rol">${u.role==='admin'?'→ user':'→ admin'}</button>
          <button class="act" data-act="rename" title="Reanomena">✎</button>
          <button class="act" data-act="token" title="Regenera token">🔑</button>
          <button class="act act-delete" data-act="del" title="Esborra"${mine?' disabled':''}>🗑</button>
        </td>`;
      tr.querySelector('[data-act=role]').onclick = async () => {
        try { await api('PATCH', '/users/' + u.id, { admin: u.role !== 'admin' }); toast('Rol actualitzat.', 'ok'); refresh(); if (mine) { await loadMe(); refreshAdminBtn(); } }
        catch (e) { toast(e.message, 'err'); }
      };
      tr.querySelector('[data-act=rename]').onclick = async () => {
        const n = prompt('Nou nom per ' + u.username + ':', u.username);
        if (!n || !n.trim()) return;
        try { await api('PATCH', '/users/' + u.id, { username: n.trim() }); toast('Nom actualitzat.', 'ok'); refresh(); }
        catch (e) { toast(e.message, 'err'); }
      };
      tr.querySelector('[data-act=token]').onclick = async () => {
        if (!confirm('Regenerar el token de ' + u.username + '? El token actual deixarà de funcionar.')) return;
        try { showToken(u.username, (await api('POST', '/users/' + u.id + '/token')).api_token); }
        catch (e) { toast(e.message, 'err'); }
      };
      const del = tr.querySelector('[data-act=del]');
      if (!mine) del.onclick = async () => {
        if (!confirm('Esborrar definitivament ' + u.username + '?')) return;
        try { await api('DELETE', '/users/' + u.id); toast('Usuari esborrat.', 'ok'); refresh(); }
        catch (e) { toast(e.message, 'err'); }
      };
      tb.appendChild(tr);
    });
  }
  fill(users);

  ov.querySelector('#nu-add').onclick = async () => {
    const name = ov.querySelector('#nu-name').value.trim();
    if (!name) { toast('Cal un nom.', 'err'); return; }
    const adm = ov.querySelector('#nu-admin').checked;
    try {
      const d = await api('POST', '/users', { username: name, admin: adm });
      ov.querySelector('#nu-name').value = '';
      ov.querySelector('#nu-admin').checked = false;
      refresh();
      showToken(d.username, d.api_token);
    } catch (e) { toast(e.message, 'err'); }
  };
}

// ---- Personalització per "cors" (sense usuaris; estat desat a cookie) ----
// La cookie guarda NOMÉS els ids marcats; el vector de l'usuari (centroide)
// es recalcula al client a partir dels embeddings dels links amb cor.
function readHearts() {
  const m = document.cookie.match(/(?:^|;\s*)clio_hearts=([^;]*)/);
  if (!m) return [];
  try { return JSON.parse(decodeURIComponent(m[1])) || []; } catch (e) { return []; }
}
function writeHearts(ids) {
  document.cookie = 'clio_hearts=' + encodeURIComponent(JSON.stringify(ids)) +
    '; path=/; max-age=31536000; SameSite=Lax';
}
let hearts = new Set(readHearts());
// Hi ha embeddings disponibles? Si no, el cor no té efecte d'ordre: l'amaguem.
const HAS_EMBED = ALL.some(l => Array.isArray(l.e) && typeof l.s === 'number');

function toggleHeart(id) {
  if (hearts.has(id)) hearts.delete(id); else hearts.add(id);
  writeHearts([...hearts]);
}

// Dequantitza l'embedding int8 d'un link -> array de floats (o null).
function vecOf(l) {
  if (!Array.isArray(l.e) || typeof l.s !== 'number') return null;
  const e = l.e, s = l.s, out = new Array(e.length);
  for (let i = 0; i < e.length; i++) out[i] = e[i] * s;
  return out;
}

// Centroide (mitjana) dels embeddings dels links amb cor. null si no n'hi ha cap.
function centroid() {
  let acc = null, n = 0;
  for (const l of ALL) {
    if (!hearts.has(l.id)) continue;
    const v = vecOf(l);
    if (!v) continue;
    if (!acc) acc = new Array(v.length).fill(0);
    for (let i = 0; i < v.length; i++) acc[i] += v[i];
    n++;
  }
  if (!acc || n === 0) return null;
  for (let i = 0; i < acc.length; i++) acc[i] /= n;
  return acc;
}

function cosine(a, b) {
  let dot = 0, na = 0, nb = 0;
  const len = Math.min(a.length, b.length);
  for (let i = 0; i < len; i++) { dot += a[i]*b[i]; na += a[i]*a[i]; nb += b[i]*b[i]; }
  if (na === 0 || nb === 0) return 0;
  return dot / (Math.sqrt(na) * Math.sqrt(nb));
}

function esc(s){ return (s||'').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }

// Renderitzador de Markdown minimal i segur: s'escapa primer l'HTML i després
// es reintrodueixen només les etiquetes generades aquí. Cobreix el subconjunt
// que produeix el LLM: titols, negreta/cursiva, codi, llistes, enllaços, cites.
function mdInline(s) {
  return esc(s)
    .replace(/`([^`]+)`/g, (_, c) => '<code>' + c + '</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[^*])\*([^*\n]+)\*/g, '$1<em>$2</em>')
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g,
      '<a href="$2" target="_blank" rel="noopener">$1</a>');
}
function md(src) {
  const lines = (src || '').split(/\r?\n/);
  const out = [];
  let inList = false, inCode = false, para = [];
  const flushPara = () => { if (para.length) { out.push('<p>' + para.join(' ') + '</p>'); para = []; } };
  const flushList = () => { if (inList) { out.push('</ul>'); inList = false; } };
  for (let raw of lines) {
    if (/^```/.test(raw)) {
      flushPara(); flushList();
      if (!inCode) { out.push('<pre><code>'); inCode = true; }
      else { out.push('</code></pre>'); inCode = false; }
      continue;
    }
    if (inCode) { out.push(esc(raw)); continue; }
    const line = raw.trim();
    if (!line) { flushPara(); flushList(); continue; }
    const h = line.match(/^(#{1,4})\s+(.*)$/);
    if (h) { flushPara(); flushList(); const n = h[1].length; out.push('<h' + n + '>' + mdInline(h[2]) + '</h' + n + '>'); continue; }
    const li = line.match(/^[-*+]\s+(.*)$/);
    if (li) { flushPara(); if (!inList) { out.push('<ul>'); inList = true; } out.push('<li>' + mdInline(li[1]) + '</li>'); continue; }
    para.push(mdInline(line));
  }
  if (inCode) out.push('</code></pre>');
  flushPara(); flushList();
  return out.join('\n');
}

// ---- Enrutament per hash: #tag:xxx o #at:usuari ----
function applyHash() {
  const h = decodeURIComponent((location.hash || '').replace(/^#/, '')).trim();
  activeTag = null; activeUser = null;
  if (h.toLowerCase().startsWith('tag:')) activeTag = h.slice(4).toLowerCase();
  else if (h.toLowerCase().startsWith('at:')) activeUser = h.slice(3).toLowerCase();
}
function setHash(h) {
  if (location.hash.replace(/^#/, '') === h) { onHashChange(); }
  else location.hash = h;
}
function onHashChange() { applyHash(); buildFilters(); render(); }

// ---- Tema fosc/clar ----
function initTheme() {
  const saved = localStorage.getItem('clio-theme');
  const sysDark = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
  const theme = saved || (sysDark ? 'dark' : 'light');
  document.documentElement.setAttribute('data-theme', theme);
  $('theme-toggle').addEventListener('click', () => {
    const next = document.documentElement.getAttribute('data-theme') === 'dark' ? 'light' : 'dark';
    document.documentElement.setAttribute('data-theme', next);
    localStorage.setItem('clio-theme', next);
  });
}

function buildFilters() {
  const counts = {};
  ALL.forEach(l => (l.tags || []).forEach(t => counts[t] = (counts[t]||0)+1));
  const top = Object.entries(counts).sort((a,b)=>b[1]-a[1] || a[0].localeCompare(b[0])).slice(0,24);
  const box = $('filters');
  box.innerHTML = '';
  // Filtre actiu per usuari (#at:): chip destacable i removible.
  if (activeUser) {
    const u = document.createElement('span');
    u.className = 'chip active';
    u.textContent = '@' + activeUser + ' ✕';
    u.title = "Enllaços enviats per " + activeUser + ' (clica per treure)';
    u.onclick = () => setHash('');
    box.appendChild(u);
  }
  top.forEach(([tag, n]) => {
    const c = document.createElement('span');
    c.className = 'chip' + (tag===activeTag ? ' active' : '');
    c.textContent = '#' + tag + ' · ' + n;
    c.onclick = () => setHash(activeTag===tag ? '' : 'tag:' + tag);
    box.appendChild(c);
  });
}

const SENT_LABEL = { positive: 'Positiu', neutral: 'Neutral', negative: 'Negatiu' };

// Bloc de la segona passada (deep): anàlisi profunda + stats de codi.
function deepBlock(l) {
  const parts = [];
  const cs = l.code_stats;
  if (cs && typeof cs === 'object') {
    const langs = (cs.top_languages || []).slice(0,4)
      .map(x => `<span class="lang">${esc(x.lang)} <i>${x.loc}</i></span>`).join('');
    parts.push(`<div class="codestats">
      <span title="Fitxers de codi">📄 ${cs.files||0}</span>
      <span title="Línies de codi">⌁ ${cs.loc||0} LOC</span>
      <span class="langs">${langs}</span>
    </div>`);
  }
  if (l.deep_summary && l.deep_status === 'done') {
    parts.push(`<details class="deep">
      <summary>🔬 Anàlisi profunda</summary>
      <div class="deep-md">${md(l.deep_summary)}</div>
    </details>`);
  } else if (l.deep_status === 'pending' || l.deep_status === 'processing') {
    parts.push(`<div class="deep-pending">🔬 Anàlisi profunda en curs…</div>`);
  }
  return parts.join('');
}

function render() {
  const q = $('search').value.trim().toLowerCase();
  const typeF = $('type-filter').value;
  const sentF = $('sent-filter').value;
  const grid = $('grid');
  grid.innerHTML = '';

  const items = ALL.filter(l => {
    if (activeTag && !(l.tags||[]).includes(activeTag)) return false;
    if (activeUser && !(l.reporters||[]).some(u => u.toLowerCase() === activeUser)) return false;
    if (onlyNew && !isNew(l)) return false;
    if (typeF && l.link_type !== typeF) return false;
    if (sentF && l.sentiment !== sentF) return false;
    if (!q) return true;
    return (l.title||'').toLowerCase().includes(q) || (l.summary||'').toLowerCase().includes(q);
  });

  // Ordre personalitzat: per afinitat (cosine) amb el centroide dels cors.
  // Sense cors, es manté l'ordre per defecte del backend (updated_at DESC).
  const cen = centroid();
  if (cen) {
    items.forEach(l => { const v = vecOf(l); l.__score = v ? cosine(v, cen) : -1; });
    items.sort((a, b) => b.__score - a.__score);
  }
  renderPerso();

  items.forEach(l => {
    const reps = l.reporters || [];
    const type = esc(l.link_type || 'other');
    const sent = esc(l.sentiment || 'neutral');
    const tags = (l.tags||[]).slice(0,8)
      .map(t => `<span class="chip" data-tag="${esc(t)}">#${esc(t)}</span>`).join('');
    const users = reps.slice(0,6)
      .map(u => `<span class="chip user" data-user="${esc(u)}">@${esc(u)}</span>`).join('');
    const card = document.createElement('article');
    card.className = 'card' + (isNew(l) ? ' is-new' : '');
    card.innerHTML = `
      <div class="card-top">
        <h2><a href="${esc(l.url)}" target="_blank" rel="noopener">${esc(l.title || l.url)}</a></h2>
        <div class="card-top-right">
          ${isNew(l) ? '<span class="badge-new" title="Nou des de la teva última visita">NOU</span>' : ''}
          <span class="badge t-${type}">${type}</span>
          ${HAS_EMBED ? `<button class="heart ${hearts.has(l.id)?'on':''}" data-id="${esc(l.id)}" title="Marca per personalitzar l'ordre" aria-label="M'agrada">♥</button>` : ''}
        </div>
      </div>
      <p class="summary">${esc(l.summary || 'Sense resum disponible.')}</p>
      <div class="tags">${tags}</div>
      ${deepBlock(l)}
      <div class="meta">
        <span class="sent ${sent}"><span class="dot"></span>${SENT_LABEL[sent] || sent}</span>
        <span class="reporters" title="Qui ha enviat aquest enllaç">${users || '👤 —'}</span>
      </div>
      ${hasToken() ? `<div class="actions">
        <button class="act act-refresh" data-id="${esc(l.id)}" title="Reforça: torna a analitzar">↻ Refer</button>
        <button class="act act-delete" data-id="${esc(l.id)}" title="Dona de baixa aquest link">🗑 Baixa</button>
      </div>` : ''}`;
    card.querySelectorAll('.tags .chip').forEach(ch => {
      ch.onclick = () => setHash('tag:' + ch.dataset.tag);
    });
    card.querySelectorAll('.reporters .user').forEach(ch => {
      ch.onclick = () => setHash('at:' + ch.dataset.user.toLowerCase());
    });
    const hb = card.querySelector('.heart');
    if (hb) hb.onclick = () => { toggleHeart(l.id); render(); };
    const rf = card.querySelector('.act-refresh');
    if (rf) rf.onclick = () => reprocessLink(l.id);
    const dl = card.querySelector('.act-delete');
    if (dl) dl.onclick = () => deleteLink(l.id);
    grid.appendChild(card);
  });

  if (!items.length) grid.innerHTML = '<div class="empty">Cap resultat amb aquests filtres.</div>';
}

// Banner d'estat de la personalització + botó de neteja.
function renderPerso() {
  const box = $('perso');
  if (!box) return;
  const n = [...hearts].filter(id => ALL.some(l => l.id === id)).length;
  if (!HAS_EMBED || !n) { box.className = 'perso'; box.innerHTML = ''; return; }
  box.className = 'perso on';
  box.innerHTML = '<span>❤ Ordenat per afinitat amb <b>' + n + '</b> ' +
    (n === 1 ? 'enllaç marcat' : 'enllaços marcats') + '</span>' +
    '<button id="perso-clear" class="perso-clear">Neteja</button>';
  $('perso-clear').onclick = () => { hearts.clear(); writeHearts([]); render(); };
}

function renderStats() {
  const total = ALL.length;
  const done = ALL.filter(l => l.status === 'done').length;
  const tags = new Set(); ALL.forEach(l => (l.tags||[]).forEach(t => tags.add(t)));
  const newCount = ALL.filter(isNew).length;
  let html =
    `<span><b>${total}</b> enllaços</span>` +
    `<span><b>${done}</b> processats</span>` +
    `<span><b>${tags.size}</b> tags</span>`;
  if (newCount) {
    html += `<span id="new-toggle" class="new-toggle${onlyNew ? ' on' : ''}" ` +
      `title="Mostra només novetats">✨ <b>${newCount}</b> novetats</span>`;
  }
  $('stats').innerHTML = html;
  const nt = $('new-toggle');
  if (nt) nt.onclick = () => { onlyNew = !onlyNew; renderStats(); render(); };
}

async function maybeFetch() {
  if (ALL.length || location.protocol === 'file:') return;
  try { const r = await fetch('data/links.json'); if (r.ok) ALL = await r.json(); } catch (e) {}
}

(async function init() {
  initTheme();
  await probeApi();
  await loadMe();
  initTokenButton();
  refreshAdminBtn();
  await maybeFetch();
  applyHash();
  renderStats();
  buildFilters();
  render();
  window.addEventListener('hashchange', onHashChange);
  $('search').addEventListener('input', render);
  $('type-filter').addEventListener('change', render);
  $('sent-filter').addEventListener('change', render);
  // Marca aquesta visita: els links nous deixaran de ser-ho a la pròxima.
  writeLastVisit(Date.now());
})();
"#;
