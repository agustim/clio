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

    let json = serde_json::to_string_pretty(&links)?;
    std::fs::write(dir.join("data/links.json"), json)?;
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
<html lang="ca">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Clio · LinkAnalyzer</title>
  <link rel="stylesheet" href="css/style.css">
</head>
<body>
  <header>
    <h1>📎 Clio</h1>
    <input id="search" type="search" placeholder="Cerca títol o resum…">
    <div id="filters"></div>
  </header>
  <main id="grid"></main>
  <footer><p>Generat per Clio · LinkAnalyzer</p></footer>
  <script src="js/app.js"></script>
</body>
</html>
"#;

const STYLE_CSS: &str = r#":root { --bg:#0f1115; --card:#1a1d24; --fg:#e6e6e6; --muted:#8a93a3; --accent:#5aa9e6; }
* { box-sizing: border-box; }
body { margin:0; font-family: system-ui, sans-serif; background:var(--bg); color:var(--fg); }
header { padding:1.2rem 1.5rem; border-bottom:1px solid #2a2e38; position:sticky; top:0; background:var(--bg); }
h1 { margin:0 0 .6rem; font-size:1.4rem; }
#search { width:100%; max-width:420px; padding:.5rem .8rem; border-radius:8px; border:1px solid #2a2e38; background:var(--card); color:var(--fg); }
#filters { margin-top:.6rem; display:flex; flex-wrap:wrap; gap:.4rem; }
.chip { cursor:pointer; font-size:.78rem; padding:.18rem .55rem; border-radius:999px; background:#252a33; color:var(--muted); border:1px solid #2f343f; }
.chip.active { background:var(--accent); color:#04121f; border-color:var(--accent); }
main { display:grid; grid-template-columns:repeat(auto-fill,minmax(280px,1fr)); gap:1rem; padding:1.5rem; }
.card { background:var(--card); border:1px solid #2a2e38; border-radius:12px; padding:1rem; display:flex; flex-direction:column; gap:.5rem; }
.card h2 { font-size:1.05rem; margin:0; }
.card h2 a { color:var(--fg); text-decoration:none; }
.card h2 a:hover { color:var(--accent); }
.badge { align-self:flex-start; font-size:.7rem; text-transform:uppercase; letter-spacing:.04em; padding:.15rem .5rem; border-radius:6px; background:#2b3340; color:var(--accent); }
.summary { color:var(--muted); font-size:.9rem; line-height:1.4; }
.tags { display:flex; flex-wrap:wrap; gap:.3rem; }
.meta { display:flex; justify-content:space-between; align-items:center; font-size:.8rem; color:var(--muted); margin-top:auto; }
.sent-positive { color:#5ad67d; } .sent-negative { color:#e65a5a; } .sent-neutral { color:var(--muted); }
footer { text-align:center; color:var(--muted); padding:2rem; font-size:.85rem; }
"#;

const APP_JS: &str = r#"let ALL = [];
let activeTag = null;

async function load() {
  const res = await fetch('data/links.json');
  ALL = await res.json();
  buildFilters();
  render();
  document.getElementById('search').addEventListener('input', render);
}

function buildFilters() {
  const counts = {};
  ALL.forEach(l => (l.tags || []).forEach(t => counts[t] = (counts[t]||0)+1));
  const top = Object.entries(counts).sort((a,b)=>b[1]-a[1]).slice(0,20);
  const box = document.getElementById('filters');
  box.innerHTML = '';
  top.forEach(([tag]) => {
    const c = document.createElement('span');
    c.className = 'chip' + (tag===activeTag?' active':'');
    c.textContent = '#' + tag;
    c.onclick = () => { activeTag = (activeTag===tag?null:tag); buildFilters(); render(); };
    box.appendChild(c);
  });
}

function esc(s){ return (s||'').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }

function render() {
  const q = document.getElementById('search').value.toLowerCase();
  const grid = document.getElementById('grid');
  grid.innerHTML = '';
  ALL.filter(l => {
    if (activeTag && !(l.tags||[]).includes(activeTag)) return false;
    if (!q) return true;
    return (l.title||'').toLowerCase().includes(q) || (l.summary||'').toLowerCase().includes(q);
  }).forEach(l => {
    const reporters = (l.co_reporters||[]).length;
    const tags = (l.tags||[]).map(t => `<span class="chip">#${esc(t)}</span>`).join('');
    const card = document.createElement('div');
    card.className = 'card';
    card.innerHTML = `
      <span class="badge">${esc(l.link_type)}</span>
      <h2><a href="${esc(l.url)}" target="_blank" rel="noopener">${esc(l.title || l.url)}</a></h2>
      <p class="summary">${esc(l.summary || '—')}</p>
      <div class="tags">${tags}</div>
      <div class="meta">
        <span class="sent-${esc(l.sentiment)}">● ${esc(l.sentiment)}</span>
        <span>👥 ${reporters}</span>
      </div>`;
    grid.appendChild(card);
  });
  if (!grid.children.length) grid.innerHTML = '<p style="color:#8a93a3">Cap resultat.</p>';
}

load();
"#;
