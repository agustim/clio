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
      <button id="theme-toggle" class="theme-toggle" aria-label="Canvia el tema" title="Canvia el tema">
        <span class="theme-icon"></span>
      </button>
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
.card h2 { font-size: 1.04rem; line-height: 1.3; margin: 0; letter-spacing: -.01em; }
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
let activeTag = null;

const $ = (id) => document.getElementById(id);
function esc(s){ return (s||'').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }

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
  top.forEach(([tag, n]) => {
    const c = document.createElement('span');
    c.className = 'chip' + (tag===activeTag ? ' active' : '');
    c.textContent = '#' + tag + ' · ' + n;
    c.onclick = () => { activeTag = (activeTag===tag ? null : tag); buildFilters(); render(); };
    box.appendChild(c);
  });
}

const SENT_LABEL = { positive: 'Positiu', neutral: 'Neutral', negative: 'Negatiu' };

function render() {
  const q = $('search').value.trim().toLowerCase();
  const typeF = $('type-filter').value;
  const sentF = $('sent-filter').value;
  const grid = $('grid');
  grid.innerHTML = '';

  const items = ALL.filter(l => {
    if (activeTag && !(l.tags||[]).includes(activeTag)) return false;
    if (typeF && l.link_type !== typeF) return false;
    if (sentF && l.sentiment !== sentF) return false;
    if (!q) return true;
    return (l.title||'').toLowerCase().includes(q) || (l.summary||'').toLowerCase().includes(q);
  });

  items.forEach(l => {
    const reporters = (l.co_reporters||[]).length;
    const type = esc(l.link_type || 'other');
    const sent = esc(l.sentiment || 'neutral');
    const tags = (l.tags||[]).slice(0,8)
      .map(t => `<span class="chip" data-tag="${esc(t)}">#${esc(t)}</span>`).join('');
    const card = document.createElement('article');
    card.className = 'card';
    card.innerHTML = `
      <div class="card-top">
        <h2><a href="${esc(l.url)}" target="_blank" rel="noopener">${esc(l.title || l.url)}</a></h2>
        <span class="badge t-${type}">${type}</span>
      </div>
      <p class="summary">${esc(l.summary || 'Sense resum disponible.')}</p>
      <div class="tags">${tags}</div>
      <div class="meta">
        <span class="sent ${sent}"><span class="dot"></span>${SENT_LABEL[sent] || sent}</span>
        <span title="Reporters">👥 ${reporters}</span>
      </div>`;
    card.querySelectorAll('.tags .chip').forEach(ch => {
      ch.onclick = () => { activeTag = ch.dataset.tag; buildFilters(); render(); };
    });
    grid.appendChild(card);
  });

  if (!items.length) grid.innerHTML = '<div class="empty">Cap resultat amb aquests filtres.</div>';
}

function renderStats() {
  const total = ALL.length;
  const done = ALL.filter(l => l.status === 'done').length;
  const tags = new Set(); ALL.forEach(l => (l.tags||[]).forEach(t => tags.add(t)));
  $('stats').innerHTML =
    `<span><b>${total}</b> enllaços</span>` +
    `<span><b>${done}</b> processats</span>` +
    `<span><b>${tags.size}</b> tags</span>`;
}

async function maybeFetch() {
  if (ALL.length || location.protocol === 'file:') return;
  try { const r = await fetch('data/links.json'); if (r.ok) ALL = await r.json(); } catch (e) {}
}

(async function init() {
  initTheme();
  await maybeFetch();
  renderStats();
  buildFilters();
  render();
  $('search').addEventListener('input', render);
  $('type-filter').addEventListener('change', render);
  $('sent-filter').addEventListener('change', render);
})();
"#;
