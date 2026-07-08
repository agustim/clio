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

    // El resum profund (deep_summary) és el camp més pesat i només es mostra
    // en obrir el detall. El treiem de l'índex i el desem per-enllaç a
    // data/deep/{id}.json, carregat mandrosament quan s'obre l'anàlisi.
    let deep_dir = dir.join("data/deep");
    if deep_dir.exists() {
        std::fs::remove_dir_all(&deep_dir)?;
    }
    std::fs::create_dir_all(&deep_dir)?;

    let mut index: Vec<serde_json::Value> = Vec::with_capacity(links.len());
    for l in &links {
        let mut v = serde_json::to_value(l)?;
        if let Some(obj) = v.as_object_mut() {
            let ds = obj.remove("deep_summary");
            if let Some(s) = ds.as_ref().and_then(|d| d.as_str()) {
                if !s.is_empty() {
                    let payload = serde_json::json!({ "deep_summary": s });
                    std::fs::write(
                        deep_dir.join(format!("{}.json", l.id)),
                        serde_json::to_string(&payload)?,
                    )?;
                }
            }
        }
        index.push(v);
    }

    let json = serde_json::to_string(&index)?;
    let json_pretty = serde_json::to_string_pretty(&index)?;
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

    // Init repo si cal. No mirem si existeix `.git` com a path perquè en un
    // container el bind-mount pot deixar-hi un dir buit o corrupte (git dona
    // llavors "not in a git directory" al primer `config`). Comprovem que sigui
    // un repo de veritat; `git init` és idempotent i arregla un `.git` incomplet.
    let is_repo = run_git(dir, &["rev-parse", "--is-inside-work-tree"]).is_ok();
    if !is_repo {
        run_git(dir, &["init"])?;
        run_git(dir, &["checkout", "-B", branch])?;
    }

    // Identitat git: sense això el commit falla en un container fresc i el push
    // peta amb "src refspec main does not match any" (no hi ha cap commit).
    run_git(dir, &["config", "user.email", "clio@localhost"])?;
    run_git(dir, &["config", "user.name", "Clio"])?;

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
    // La web és contingut derivat de la BD (regenerat sencer cada cop) i Clio
    // és l'únic que escriu en aquest remot, que és només destí de publicació.
    // Sobreescrivim amb --force per no fallar amb "fetch first" si divergeix.
    run_git(dir, &["push", "-u", "--force", "origin", branch])?;
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
    // `safe.directory=*` desactiva el check d'ownership de git: dins el container
    // el procés corre com a root però els fitxers de public/ són d'un altre uid
    // (bind-mount), i git peta amb "detected dubious ownership". Aquí no hi ha
    // risc: és un repo derivat i d'un sol escriptor.
    let out = Command::new("git")
        .args(["-c", "safe.directory=*"])
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
      <div class="menu-wrap">
        <button id="menu-btn" class="theme-toggle" aria-label="Menú" title="Menú" aria-haspopup="true" aria-expanded="false">☰</button>
        <div id="menu" class="menu" role="menu" hidden>
          <button class="menu-item" data-act="search" role="menuitem"><span class="mi-check"></span> Mostra el cercador</button>
          <button class="menu-item" data-act="tags" role="menuitem"><span class="mi-check"></span> Mostra els tags</button>
          <button class="menu-item" data-act="new" role="menuitem"><span class="mi-check"></span> Mostra novetats</button>
          <button class="menu-item" data-act="new-dec" role="menuitem"><span class="mi-ico">◀</span> Novetats: un dia abans</button>
          <button class="menu-item" data-act="new-inc" role="menuitem"><span class="mi-ico">▶</span> Novetats: un dia després</button>
          <div class="menu-sep"></div>
          <button class="menu-item" data-act="cols-dec" role="menuitem"><span class="mi-ico">−</span> Menys columnes</button>
          <button class="menu-item" data-act="cols-inc" role="menuitem"><span class="mi-ico">+</span> Més columnes</button>
          <div class="menu-sep"></div>
          <button class="menu-item" data-act="theme" role="menuitem"><span class="mi-ico theme-icon"></span> Canvia el tema</button>
          <div class="menu-sep" id="menu-api-sep" hidden></div>
          <button class="menu-item" id="mi-token" data-act="token" role="menuitem" hidden>🔑 Sessió</button>
          <button class="menu-item" id="mi-admin" data-act="admin" role="menuitem" hidden>👤 Usuaris</button>
          <button class="menu-item" id="mi-add" data-act="add" role="menuitem" hidden>➕ Afegeix enllaç</button>
        </div>
      </div>
    </div>
    <div id="controls" class="topbar-inner controls">
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
        <option value="social">Social</option>
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
.cols-btn { font-size: 1.3rem; font-weight: 600; line-height: 1; }

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

.filters { max-width: var(--maxw); margin: .7rem auto 0; display: none; flex-wrap: wrap; gap: .4rem; }
.filters.open { display: flex; }
.tags-toggle { cursor: pointer; user-select: none; transition: color var(--transition); }
.tags-toggle:hover { color: var(--fg); }
.tags-toggle.on, .tags-toggle.on b { color: var(--accent); }

/* ---- Menú (☰) ---- */
.menu-wrap { position: relative; }
.menu {
  position: absolute; right: 0; top: calc(100% + .45rem); min-width: 244px;
  background: var(--card); border: 1px solid var(--border); border-radius: 12px;
  box-shadow: var(--shadow); padding: .35rem; z-index: 60;
  display: flex; flex-direction: column; gap: .05rem;
}
.menu[hidden] { display: none; }
.menu-item {
  display: flex; align-items: center; gap: .55rem; width: 100%; text-align: left;
  cursor: pointer; font-size: .86rem; padding: .5rem .55rem; border-radius: 8px;
  border: 1px solid transparent; background: none; color: var(--fg);
  transition: background var(--transition), color var(--transition);
}
.menu-item:hover { background: var(--card-hover); }
.menu-item[hidden] { display: none; }
.menu-item.on { color: var(--accent); }
.menu-item .mi-check, .menu-item .mi-ico {
  flex: none; width: 1.1em; text-align: center; color: var(--muted);
}
.menu-item.on .mi-check { color: var(--accent); }
.menu-item.on .mi-check::before { content: "✓"; }
.menu-sep { height: 1px; background: var(--border); margin: .28rem .35rem; }
.menu-sep[hidden] { display: none; }

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
.card-top { display: flex; align-items: center; justify-content: center; gap: .5rem; }
.card h2 { font-size: 1.04rem; line-height: 1.3; margin: 0; letter-spacing: -.01em; text-align: center;
  display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }
.card h2 a { color: var(--fg); text-decoration: none; }
.card h2 a:hover { color: var(--accent); }

/* Fila 2: tipus + cor a l'esquerra, pestanyes I/A/D a la dreta */
.card-row2 { display: flex; align-items: center; justify-content: space-between; gap: .5rem; }
.card-row2-left { display: flex; align-items: center; gap: .45rem; min-width: 0; }
.card-tabs { display: flex; gap: .3rem; flex: none; }
.card-tabs .tab {
  width: 30px; height: 30px; border-radius: 8px; cursor: pointer; font-size: .9rem;
  border: 1px solid var(--border); background: var(--card); color: var(--fg);
  display: grid; place-items: center; transition: all var(--transition);
}
.card-tabs .tab:hover { background: var(--card-hover); }
.card-tabs .tab.on { border-color: var(--accent); color: var(--accent); background: color-mix(in srgb, var(--accent) 14%, transparent); }
.card-tabs .tab:disabled { opacity: .35; cursor: not-allowed; }
.card-panels { min-height: 1px; }
.card-panels .panel[hidden] { display: none; }

.badge {
  flex: none; font-size: .66rem; font-weight: 600; text-transform: uppercase; letter-spacing: .05em;
  padding: .2rem .5rem; border-radius: 6px; border: 1px solid transparent;
}
.badge.t-news    { color: #ff9f43; background: color-mix(in srgb, #ff9f43 16%, transparent); }
.badge.t-repo    { color: #8b7dff; background: color-mix(in srgb, #8b7dff 16%, transparent); }
.badge.t-article { color: var(--accent); background: color-mix(in srgb, var(--accent) 16%, transparent); }
.badge.t-video   { color: #ff6b7a; background: color-mix(in srgb, #ff6b7a 16%, transparent); }
.badge.t-blog    { color: #45d49a; background: color-mix(in srgb, #45d49a 16%, transparent); }
.badge.t-social  { color: #4aa8ff; background: color-mix(in srgb, #4aa8ff 16%, transparent); }
.badge.t-other   { color: var(--muted); background: var(--bg-soft); }

.summary { color: var(--muted); font-size: .9rem; line-height: 1.5; margin: 0; }
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

/* Detalls de la targeta (tags + info) plegables */
.card-extra { border-top: 1px dashed var(--border); padding-top: .5rem; }
.card-extra > summary { cursor: pointer; color: var(--muted); font-size: .82rem; font-weight: 500; user-select: none; }
.card-extra > summary:hover { color: var(--fg); }
.card-extra[open] > summary { margin-bottom: .5rem; }
.card-extra .tags { margin-bottom: .5rem; }
.card-extra .meta { margin-top: 0; }
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
.act-block:hover { color: var(--neg); border-color: var(--neg); }

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
.ucreate #nu-name, .ucreate #nu-tg {
  flex: 1 1 160px; padding: .45rem .7rem; border-radius: 8px;
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

/* Enllaç permanent per card (#id:xxx): icona discreta a la fila de tipus. */
.permalink { flex: none; text-decoration: none; font-size: .82rem; line-height: 1; opacity: .45; transition: opacity var(--transition); }
.permalink:hover { opacity: 1; }

/* Vista permalink: una sola card centrada, sense filtres ni estadístiques. */
body.single-view #controls,
body.single-view #stats,
body.single-view #perso,
body.single-view #filters { display: none; }
body.single-view .grid { display: block; max-width: 640px; margin: 0 auto; }
.home-link { text-align: center; padding: 2rem 1rem 1rem; }
.home-link a { color: var(--accent); text-decoration: none; font-size: .9rem; }
.home-link a:hover { text-decoration: underline; }

@media (max-width: 560px) {
  .grid { grid-template-columns: minmax(0, 1fr); }
  .tagline { display: none; }
}
"#;

const APP_JS: &str = r##""use strict";

// Dades: incrustades a data/links.js (window.__LINKS__) per funcionar via file://.
// Fallback a fetch si s'està servint per HTTP i no hi ha incrustat.
let ALL = Array.isArray(window.__LINKS__) ? window.__LINKS__ : [];
let activeTag = null;   // filtre per tag (#tag:xxx)
let activeUser = null;  // filtre per reporter (#at:xxx)
let activeId = null;    // permalink: mostra només una card (#id:xxx)
let onlyNew = false;    // mostra només novetats (links no vistos)
let filtersOpen = false; // llistat de tags general plegat per defecte

// ---- Cerca: amagada per defecte, l'estat es recorda a cookie ----
function readSearchOpen() { return /(?:^|;\s*)clio_search=1/.test(document.cookie); }
function writeSearchOpen(v) { document.cookie = 'clio_search=' + (v ? 1 : 0) + '; path=/; max-age=31536000; SameSite=Lax'; }
let searchOpen = readSearchOpen();

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
// Desplaçament del llindar de novetats en dies (persistit). Negatiu = enrere
// (mostra'n més, també les més antigues); positiu = endavant (mostra'n menys).
function readNewOffset() { const m = document.cookie.match(/(?:^|;\s*)clio_newoff=(-?\d+)/); return m ? parseInt(m[1], 10) : 0; }
function writeNewOffset(n) { document.cookie = 'clio_newoff=' + n + '; path=/; max-age=31536000; SameSite=Lax'; }
let NEW_OFFSET = readNewOffset();
function newSince() { return LAST_VISIT + NEW_OFFSET * 86400000; }
function isNew(l) { return LAST_VISIT > 0 && linkTime(l) > newSince(); }

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
  // No n'hi ha prou amb r.ok: hostings estàtics (Cloudflare Pages, etc.) tornen
  // 200 amb l'HTML de fallback per a rutes desconegudes. Cal confirmar que el cos
  // és el JSON del servei viu ({ serve: true }).
  try {
    const r = await fetch('/api/v1/ping', { cache: 'no-store' });
    if (!r.ok) { API_LIVE = false; return; }
    const j = await r.json().catch(() => null);
    API_LIVE = !!(j && j.serve === true);
  } catch (e) { API_LIVE = false; }
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
async function blockLink(id) {
  if (!confirm('Bloquejar aquest URL? S\'afegirà a la blocklist i el link s\'esborrarà.')) return;
  try {
    await api('POST', '/links/' + id + '/block');
    ALL = ALL.filter(l => l.id !== id);
    hearts.delete(id);
    renderStats(); buildFilters(); render();
    toast('URL bloquejada i link esborrat.', 'ok');
  } catch (e) { toast('Error en bloquejar: ' + e.message, 'err'); }
}

// Actualitza la visibilitat i l'estat dels ítems del menú lligats a l'API
// (només tenen sentit contra un servei viu). S'invoca en canviar de sessió.
function refreshApiItems() {
  const tk = $('mi-token'), ad = $('mi-admin'), af = $('mi-add'), sep = $('menu-api-sep');
  if (!tk) return;
  tk.hidden = !API_LIVE;
  tk.classList.toggle('on', !!getToken());
  tk.textContent = getToken() ? '🔑 Sessió activa (tanca / canvia)' : '🔑 Inicia sessió amb token';
  if (ad) ad.hidden = !isAdmin();
  if (af) af.hidden = !hasToken();
  if (sep) sep.hidden = !API_LIVE;
}

async function promptToken() {
  if (!API_LIVE) { toast('Aquesta web no té servei API actiu.', 'err'); return; }
  const cur = getToken();
  const t = prompt(cur ? 'API token (buit per tancar sessió):' : 'Enganxa el teu API token:', cur);
  if (t === null) return;
  setToken(t.trim());
  await loadMe();
  refreshApiItems(); render();
  toast(getToken() ? 'Sessió iniciada.' : 'Sessió tancada.', 'ok');
}

// ---- Afegir enllaç (qualsevol usuari amb token) ----

async function addLink() {
  if (!hasToken()) { toast('Cal iniciar sessió amb un token.', 'err'); return; }
  const raw = prompt('URL del nou enllaç (pots enganxar-ne diversos separats per espais):');
  if (raw === null) return;
  const urls = raw.split(/\s+/).map(s => s.trim()).filter(Boolean);
  if (!urls.length) { toast('Cap URL.', 'err'); return; }
  try {
    const res = await api('POST', '/links', urls.length === 1 ? { url: urls[0] } : { urls });
    // Resposta única (un enllaç) o lot amb { results }.
    const ids = res.results
      ? res.results.filter(r => r.link_id).map(r => r.link_id)
      : (res.link_id ? [res.link_id] : []);
    // Prepend dels nous links perquè apareguin sense recarregar (encara "pending").
    for (const id of ids) {
      try { const l = await api('GET', '/links/' + id); if (l && l.id && !ALL.some(x => x.id === l.id)) ALL.unshift(l); }
      catch (e) {}
    }
    renderStats(); buildFilters(); render();
    toast(ids.length === 1 ? "Enllaç afegit: s'analitzarà en breu." : ids.length + ' enllaços afegits.', 'ok');
  } catch (e) { toast('Error afegint: ' + e.message, 'err'); }
}

// ---- Admin: gestió d'usuaris ----
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
        <thead><tr><th>Usuari</th><th>Rol</th><th>Telegram</th><th>Creat</th><th></th></tr></thead>
        <tbody id="ulist"></tbody>
      </table>
      <div class="ucreate">
        <input id="nu-name" placeholder="nom del nou usuari" autocomplete="off">
        <input id="nu-tg" placeholder="telegram id (opcional)" autocomplete="off">
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
      const tg = u.telegram_id ? esc(u.telegram_id) : '<span class="you">—</span>';
      tr.innerHTML = `<td>${esc(u.username)}${mine ? ' <span class="you">(tu)</span>' : ''}</td>
        <td><span class="rolebadge ${u.role}">${u.role}</span></td>
        <td class="tgcell">${tg}</td>
        <td>${(u.created_at || '').slice(0,10)}</td>
        <td class="urow-actions">
          <button class="act" data-act="role" title="Canvia el rol">${u.role==='admin'?'→ user':'→ admin'}</button>
          <button class="act" data-act="rename" title="Reanomena">✎</button>
          <button class="act" data-act="tg" title="Edita telegram id">✈</button>
          <button class="act" data-act="token" title="Regenera token">🔑</button>
          <button class="act act-delete" data-act="del" title="Esborra"${mine?' disabled':''}>🗑</button>
        </td>`;
      tr.querySelector('[data-act=role]').onclick = async () => {
        try { await api('PATCH', '/users/' + u.id, { admin: u.role !== 'admin' }); toast('Rol actualitzat.', 'ok'); refresh(); if (mine) { await loadMe(); refreshApiItems(); } }
        catch (e) { toast(e.message, 'err'); }
      };
      tr.querySelector('[data-act=rename]').onclick = async () => {
        const n = prompt('Nou nom per ' + u.username + ':', u.username);
        if (!n || !n.trim()) return;
        try { await api('PATCH', '/users/' + u.id, { username: n.trim() }); toast('Nom actualitzat.', 'ok'); refresh(); }
        catch (e) { toast(e.message, 'err'); }
      };
      tr.querySelector('[data-act=tg]').onclick = async () => {
        const v = prompt('Telegram id de ' + u.username + ' (buit per treure):', u.telegram_id || '');
        if (v === null) return;
        try { await api('PATCH', '/users/' + u.id, { telegram_id: v.trim() }); toast('Telegram id actualitzat.', 'ok'); refresh(); }
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
    const tg = ov.querySelector('#nu-tg').value.trim();
    try {
      const d = await api('POST', '/users', { username: name, admin: adm, telegram_id: tg });
      ov.querySelector('#nu-name').value = '';
      ov.querySelector('#nu-tg').value = '';
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

// Resum curt: explicació de què és, màxim 150 caràcters, sense punts suspensius.
// L'anàlisi profunda cobreix el text llarg.
function summaryText(s) {
  const t = (s || '').trim();
  if (!t) return 'Sense resum disponible.';
  return t.length > 150 ? t.slice(0, 150).trimEnd() : t;
}

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
  activeTag = null; activeUser = null; activeId = null;
  if (h.toLowerCase().startsWith('tag:')) activeTag = h.slice(4).toLowerCase();
  else if (h.toLowerCase().startsWith('at:')) activeUser = h.slice(3).toLowerCase();
  else if (h.toLowerCase().startsWith('id:')) activeId = h.slice(3);
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
}
function toggleTheme() {
  const next = document.documentElement.getAttribute('data-theme') === 'dark' ? 'light' : 'dark';
  document.documentElement.setAttribute('data-theme', next);
  localStorage.setItem('clio-theme', next);
}

// ---- Nombre de columnes de la graella (desat a cookie) ----
// 0 = automàtic (per defecte del CSS). >0 força N columnes.
function readCols() {
  const m = document.cookie.match(/(?:^|;\s*)clio_cols=(\d+)/);
  return m ? parseInt(m[1], 10) : 0;
}
function writeCols(n) {
  document.cookie = 'clio_cols=' + n + '; path=/; max-age=31536000; SameSite=Lax';
}
function applyCols() {
  const n = readCols();
  const g = $('grid');
  g.style.gridTemplateColumns = n > 0 ? `repeat(${n}, minmax(0, 1fr))` : '';
}
// Columnes actuals: la cookie si hi és, altrament les que calcula el CSS auto-fill.
function curCols() {
  const n = readCols();
  if (n > 0) return n;
  const cs = getComputedStyle($('grid')).gridTemplateColumns;
  return cs && cs !== 'none' ? cs.split(' ').length : 1;
}
function colsInc() { writeCols(Math.min(curCols() + 1, 8)); applyCols(); }
function colsDec() { writeCols(Math.max(curCols() - 1, 1)); applyCols(); }
function initCols() { applyCols(); }

function buildFilters() {
  const counts = {};
  ALL.forEach(l => (l.tags || []).forEach(t => counts[t] = (counts[t]||0)+1));
  const top = Object.entries(counts).sort((a,b)=>b[1]-a[1] || a[0].localeCompare(b[0])).slice(0,24);
  const box = $('filters');
  box.innerHTML = '';
  // Llistat general plegat per defecte: només visible si s'obre des del toggle
  // de tags o si hi ha un filtre actiu (per poder treure'l).
  const show = filtersOpen || activeTag || activeUser;
  box.classList.toggle('open', !!show);
  if (!show) return;
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

// Info del repositori o del vídeo (stats): es mostra dins "Detalls".
function repoBlock(l) {
  const cs = l.code_stats;
  if (!cs || typeof cs !== 'object') return '';
  // Vídeos: canal + durada + si té transcripció.
  if (cs.channel !== undefined || cs.duration_secs !== undefined) {
    const parts = [];
    if (cs.channel) parts.push(`<span title="Canal">📺 ${esc(cs.channel)}</span>`);
    if (cs.duration_secs) parts.push(`<span title="Durada">⏱ ${fmtDur(cs.duration_secs)}</span>`);
    if (cs.has_transcript) parts.push(`<span title="Transcripció disponible">📝 transcripció</span>`);
    return `<div class="codestats">${parts.join('')}</div>`;
  }
  // Repos: fitxers + LOC + llenguatges.
  const langs = (cs.top_languages || []).slice(0,4)
    .map(x => `<span class="lang">${esc(x.lang)} <i>${x.loc}</i></span>`).join('');
  return `<div class="codestats">
      <span title="Fitxers de codi">📄 ${cs.files||0}</span>
      <span title="Línies de codi">⌁ ${cs.loc||0} LOC</span>
      <span class="langs">${langs}</span>
    </div>`;
}

function fmtDur(s) {
  s = parseInt(s, 10) || 0;
  const h = Math.floor(s/3600), m = Math.floor((s%3600)/60), sec = s%60;
  return (h ? h+'h ' : '') + (m ? m+'m ' : '') + (h ? '' : sec+'s');
}

// Bloc de la segona passada (deep): anàlisi profunda en text.
// El text pesat viu a data/deep/{id}.json i es carrega mandrosament en obrir.
function deepPanel(l) {
  if (l.deep_status === 'done') {
    return `<div class="deep-md" data-deep="${esc(l.id)}"><span class="deep-loading">Carregant…</span></div>`;
  } else if (l.deep_status === 'pending' || l.deep_status === 'processing') {
    return `<div class="deep-pending">🔬 Anàlisi profunda en curs…</div>`;
  }
  return `<div class="deep-pending">Sense anàlisi profunda.</div>`;
}
function deepAvailable(l) { return l.deep_status === 'done'; }

// Cache i càrrega mandrosa del resum profund (un fetch per enllaç, un sol cop).
const DEEP_CACHE = new Map();
async function loadDeep(box) {
  const id = box.dataset.deep;
  if (!id || box.dataset.loaded) return;
  box.dataset.loaded = '1';
  let text = DEEP_CACHE.get(id);
  if (text === undefined) {
    try {
      const r = await fetch('data/deep/' + id + '.json');
      text = r.ok ? ((await r.json()).deep_summary || '') : '';
    } catch (e) { text = ''; }
    DEEP_CACHE.set(id, text);
  }
  box.innerHTML = text ? md(text) : '<span class="deep-loading">No disponible.</span>';
}

function render() {
  const q = $('search').value.trim().toLowerCase();
  const typeF = $('type-filter').value;
  const sentF = $('sent-filter').value;
  const grid = $('grid');
  grid.innerHTML = '';

  // Vista permalink (#id:xxx): només una card, sense filtres ni estadístiques.
  document.body.classList.toggle('single-view', !!activeId);
  if (activeId) { renderSingle(grid); return; }

  const items = ALL.filter(l => {
    if (activeTag && !(l.tags||[]).includes(activeTag)) return false;
    if (activeUser && !(l.reporters||[]).some(u => u.toLowerCase() === activeUser)) return false;
    if (typeF && l.link_type !== typeF) return false;
    if (sentF && l.sentiment !== sentF) return false;
    if (!q) return true;
    return (l.title||'').toLowerCase().includes(q) || (l.summary||'').toLowerCase().includes(q);
  });

  // Ordre personalitzat: per afinitat (cosine) amb el centroide dels cors.
  // Sense cors, es manté l'ordre per defecte del backend (updated_at DESC).
  const cen = centroid();
  if (cen) items.forEach(l => { const v = vecOf(l); l.__score = v ? cosine(v, cen) : -1; });
  if (onlyNew || cen) {
    items.sort((a, b) => {
      // Amb "novetats" actiu: nous primer, antics després; cada grup per interès.
      if (onlyNew) { const d = (isNew(b)?1:0) - (isNew(a)?1:0); if (d) return d; }
      return cen ? b.__score - a.__score : 0;
    });
  }
  renderPerso();

  items.forEach(l => { grid.appendChild(buildCard(l)); });

  if (!items.length) grid.innerHTML = '<div class="empty">Cap resultat amb aquests filtres.</div>';
}

// Vista d'una sola card via permalink (#id:xxx). Afegeix un enllaç a l'inici.
function renderSingle(grid) {
  const l = ALL.find(x => x.id === activeId);
  if (l) grid.appendChild(buildCard(l));
  else grid.innerHTML = '<div class="empty">Aquest enllaç no existeix o s\'ha donat de baixa.</div>';
  const home = document.createElement('div');
  home.className = 'home-link';
  home.innerHTML = '<a href="#">← Torna a tots els enllaços</a>';
  home.querySelector('a').onclick = (e) => { e.preventDefault(); setHash(''); };
  grid.appendChild(home);
}

// Construeix l'element <article> d'una card. Reutilitzat per la graella i el permalink.
function buildCard(l) {
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
      </div>
      <div class="card-row2">
        <div class="card-row2-left">
          ${isNew(l) ? '<span class="badge-new" title="Nou des de la teva última visita">NOU</span>' : ''}
          <span class="badge t-${type}">${type}</span>
          <a class="permalink" href="#id:${esc(l.id)}" title="Enllaç permanent a aquesta card" aria-label="Enllaç permanent">🔗</a>
          ${HAS_EMBED ? `<button class="heart ${hearts.has(l.id)?'on':''}" data-id="${esc(l.id)}" title="Marca per personalitzar l'ordre" aria-label="M'agrada">♥</button>` : ''}
        </div>
        <div class="card-tabs">
          <button class="tab on" data-panel="info" title="Info">ℹ️</button>
          <button class="tab" data-panel="deep" title="Anàlisi profunda"${deepAvailable(l) ? '' : ' disabled'}>🔬</button>
          <button class="tab" data-panel="details" title="Detalls">📋</button>
        </div>
      </div>
      <div class="card-panels">
        <div class="panel" data-panel="info"><p class="summary">${esc(summaryText(l.summary))}</p></div>
        <div class="panel" data-panel="deep" hidden>${deepPanel(l)}</div>
        <div class="panel" data-panel="details" hidden>
          ${repoBlock(l)}
          <div class="tags">${tags}</div>
          <div class="meta">
            <span class="sent ${sent}"><span class="dot"></span>${SENT_LABEL[sent] || sent}</span>
            <span class="reporters" title="Qui ha enviat aquest enllaç">${users || '👤 —'}</span>
          </div>
        </div>
      </div>
      ${hasToken() ? `<div class="actions">
        <button class="act act-refresh" data-id="${esc(l.id)}" title="Reforça: torna a analitzar">↻ Refer</button>
        <button class="act act-delete" data-id="${esc(l.id)}" title="Dona de baixa aquest link">🗑 Baixa</button>
        ${isAdmin() ? `<button class="act act-block" data-id="${esc(l.id)}" title="Bloqueja aquest URL: l'afegeix a la blocklist i esborra el link">🚫 Bloqueja</button>` : ''}
      </div>` : ''}`;
    card.querySelectorAll('.tags .chip').forEach(ch => {
      ch.onclick = () => setHash('tag:' + ch.dataset.tag);
    });
    card.querySelectorAll('.reporters .user').forEach(ch => {
      ch.onclick = () => setHash('at:' + ch.dataset.user.toLowerCase());
    });
    const tabs = card.querySelectorAll('.card-tabs .tab');
    const panels = card.querySelectorAll('.card-panels .panel');
    tabs.forEach(t => t.addEventListener('click', () => {
      if (t.disabled) return;
      const name = t.dataset.panel;
      tabs.forEach(x => x.classList.toggle('on', x === t));
      panels.forEach(p => p.hidden = p.dataset.panel !== name);
      if (name === 'deep') {
        const box = card.querySelector('.panel[data-panel="deep"] .deep-md');
        if (box) loadDeep(box);
      }
    }));
    const hb = card.querySelector('.heart');
    if (hb) hb.onclick = () => { toggleHeart(l.id); render(); };
    const rf = card.querySelector('.act-refresh');
    if (rf) rf.onclick = () => reprocessLink(l.id);
    const dl = card.querySelector('.act-delete');
    if (dl) dl.onclick = () => deleteLink(l.id);
    const bl = card.querySelector('.act-block');
    if (bl) bl.onclick = () => blockLink(l.id);
    const pl = card.querySelector('.permalink');
    if (pl) pl.onclick = (e) => { e.preventDefault(); setHash('id:' + l.id); };
    return card;
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
    `<span id="tags-toggle" class="tags-toggle${filtersOpen ? ' on' : ''}" ` +
      `title="Mostra/amaga el llistat de tags"># <b>${tags.size}</b> tags</span>`;
  if (newCount) {
    html += `<span id="new-toggle" class="new-toggle${onlyNew ? ' on' : ''}" ` +
      `title="Mostra només novetats">✨ <b>${newCount}</b> novetats</span>`;
  }
  $('stats').innerHTML = html;
  const tt = $('tags-toggle');
  if (tt) tt.onclick = () => { filtersOpen = !filtersOpen; renderStats(); buildFilters(); updateMenuState(); };
  const nt = $('new-toggle');
  if (nt) nt.onclick = () => { onlyNew = !onlyNew; renderStats(); render(); updateMenuState(); };
}

// ---- Menú (☰): cercador, tags, novetats, columnes, tema, accions API ----
function applySearch() { const c = $('controls'); if (c) c.style.display = searchOpen ? '' : 'none'; }

function shiftNew(days) {
  NEW_OFFSET += days;
  writeNewOffset(NEW_OFFSET);
  renderStats(); render();
  const label = NEW_OFFSET === 0 ? "a l'última visita"
    : (NEW_OFFSET > 0 ? '+' : '') + NEW_OFFSET + (Math.abs(NEW_OFFSET) === 1 ? ' dia' : ' dies') + ' respecte l’última visita';
  toast('Llindar de novetats: ' + label + '.', 'ok');
}

function updateMenuState() {
  const set = (act, on) => { const el = document.querySelector('.menu-item[data-act="' + act + '"]'); if (el) el.classList.toggle('on', !!on); };
  set('search', searchOpen); set('tags', filtersOpen); set('new', onlyNew);
}

function menuAction(act) {
  switch (act) {
    case 'search': searchOpen = !searchOpen; writeSearchOpen(searchOpen); applySearch(); updateMenuState(); break;
    case 'tags': filtersOpen = !filtersOpen; renderStats(); buildFilters(); updateMenuState(); break;
    case 'new': onlyNew = !onlyNew; renderStats(); render(); updateMenuState(); break;
    case 'new-dec': shiftNew(-1); break;
    case 'new-inc': shiftNew(1); break;
    case 'cols-dec': colsDec(); break;
    case 'cols-inc': colsInc(); break;
    case 'theme': toggleTheme(); break;
    case 'token': promptToken(); break;
    case 'admin': openUsersModal(); break;
    case 'add': addLink(); break;
  }
}

function initMenu() {
  const btn = $('menu-btn'), menu = $('menu');
  if (!btn || !menu) return;
  const close = () => { menu.hidden = true; btn.setAttribute('aria-expanded', 'false'); };
  const open = () => { menu.hidden = false; btn.setAttribute('aria-expanded', 'true'); updateMenuState(); };
  btn.addEventListener('click', (e) => { e.stopPropagation(); menu.hidden ? open() : close(); });
  menu.addEventListener('click', (e) => {
    const it = e.target.closest('.menu-item'); if (!it) return;
    const act = it.dataset.act;
    // Les accions que obren un prompt/modal tanquen el menú; els toggles el deixen obert.
    if (act === 'token' || act === 'admin' || act === 'add') close();
    menuAction(act);
  });
  document.addEventListener('click', (e) => { if (!menu.hidden && !menu.contains(e.target) && e.target !== btn) close(); });
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape') close(); });
  applySearch();
  refreshApiItems();
  updateMenuState();
}

async function maybeFetch() {
  if (ALL.length || location.protocol === 'file:') return;
  try { const r = await fetch('data/links.json'); if (r.ok) ALL = await r.json(); } catch (e) {}
}

(async function init() {
  initTheme();
  initCols();
  await probeApi();
  await loadMe();
  initMenu();
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
"##;
