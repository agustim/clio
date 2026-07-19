use crate::config::Config;
use crate::db::Db;
use crate::error::{AppError, Result};
use std::path::Path;
use std::process::Command;

/// Mida màxima (links) d'un part dins d'un mes. Fonts amb molt volum (NPCs)
/// queden en trossos descarregables progressivament.
const PART_SIZE: usize = 200;

/// Nom de fitxer segur per a un username (el client el rep com a `dir` al manifest).
fn user_dir(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Genera la web estatica a cfg.public_dir.
///
/// Disposició de dades (per usuari/font, pensada per "seguir N fonts"):
///  - data/manifest.json                     — total + fonts (mesos, parts, emb) + categories
///  - data/u/{font}/{YYYY-MM}-p{N}.json      — índex lleuger per font, mes i part
///  - data/u/{font}/emb-{YYYY-MM}-p{N}.json  — embeddings alineats al part (lazy amb els cors)
///  - data/i/{id}.json                       — fitxa lleugera per enllaç (permalinks)
///  - data/deep/{id}.json                    — resum profund (lazy en obrir el detall)
///  - data/links.json / links.js             — índex lleuger complet: consum extern + fallback file://
pub async fn generate(db: &Db, cfg: &Config) -> Result<()> {
    let links = db.list_links(None, None, None, 100_000).await?;
    let users = db.list_users().await?;
    let dir = Path::new(&cfg.public_dir);
    std::fs::create_dir_all(dir.join("data"))?;
    std::fs::create_dir_all(dir.join("css"))?;
    std::fs::create_dir_all(dir.join("js"))?;

    // Directoris derivats: es regeneren sencers a cada `generate`.
    // months/ i emb/ són el layout antic (per mes global): es netegen si queden.
    for sub in ["data/deep", "data/u", "data/i", "data/months", "data/emb"] {
        let d = dir.join(sub);
        if d.exists() {
            std::fs::remove_dir_all(&d)?;
        }
    }
    for sub in ["data/deep", "data/u", "data/i"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    let deep_dir = dir.join("data/deep");

    // L'índex lleuger treu els camps pesats o interns:
    //  - deep_summary -> data/deep/{id}.json (lazy en obrir l'anàlisi)
    //  - e/s (embedding quantitzat) -> data/u/{font}/emb-*.json (lazy amb els cors)
    //  - co_reporters (uuids interns) -> la web usa `reporters` (noms)
    let mut index: Vec<serde_json::Value> = Vec::with_capacity(links.len());
    // (ts, id, light, emb) per repartir per font; un link co-reportat va al shard
    // de cada reporter (dedup per id al client).
    let mut rows: Vec<(i64, String, serde_json::Value, Option<serde_json::Value>)> =
        Vec::with_capacity(links.len());
    for l in &links {
        let mut v = serde_json::to_value(l)?;
        let mut emb = None;
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
            if let (Some(e), Some(s)) = (obj.remove("e"), obj.remove("s")) {
                emb = Some(serde_json::json!({ "e": e, "s": s }));
            }
            obj.remove("co_reporters");
        }
        std::fs::write(
            dir.join(format!("data/i/{}.json", l.id)),
            serde_json::to_string(&v)?,
        )?;
        rows.push((l.created_at.timestamp(), l.id.to_string(), v.clone(), emb));
        index.push(v);
    }

    // Reparteix per font (reporter). Ordre dins de cada font: més recent primer.
    let mut per_user: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, l) in links.iter().enumerate() {
        for rep in &l.reporters {
            per_user.entry(rep.clone()).or_default().push(i);
        }
    }
    let role_of: std::collections::HashMap<&str, String> = users
        .iter()
        .map(|u| (u.username.as_str(), format!("{:?}", u.role).to_lowercase()))
        .collect();

    let mut user_entries: Vec<serde_json::Value> = Vec::new();
    for (name, mut idxs) in per_user {
        idxs.sort_by_key(|&i| -rows[i].0);
        let udir = user_dir(&name);
        std::fs::create_dir_all(dir.join("data/u").join(&udir))?;
        let has_emb = idxs.iter().any(|&i| rows[i].3.is_some());

        // Agrupa per mes conservant l'ordre (desc) i talla en parts de PART_SIZE.
        let mut month_list: Vec<serde_json::Value> = Vec::new();
        let mut months: Vec<(String, Vec<usize>)> = Vec::new();
        for &i in &idxs {
            let month = chrono::DateTime::from_timestamp(rows[i].0, 0)
                .map(|d| d.format("%Y-%m").to_string())
                .unwrap_or_else(|| "0000-00".into());
            match months.last_mut() {
                Some((k, v)) if *k == month => v.push(i),
                _ => months.push((month, vec![i])),
            }
        }
        for (key, mids) in &months {
            let parts: Vec<&[usize]> = mids.chunks(PART_SIZE).collect();
            for (p, chunk) in parts.iter().enumerate() {
                let arr: Vec<&serde_json::Value> = chunk.iter().map(|&i| &rows[i].2).collect();
                std::fs::write(
                    dir.join(format!("data/u/{udir}/{key}-p{p}.json")),
                    serde_json::to_string(&arr)?,
                )?;
                if has_emb {
                    let mut map = serde_json::Map::new();
                    for &i in chunk.iter() {
                        if let Some(e) = &rows[i].3 {
                            map.insert(rows[i].1.clone(), e.clone());
                        }
                    }
                    std::fs::write(
                        dir.join(format!("data/u/{udir}/emb-{key}-p{p}.json")),
                        serde_json::to_string(&map)?,
                    )?;
                }
            }
            month_list.push(serde_json::json!({
                "key": key,
                "count": mids.len(),
                "parts": parts.len(),
            }));
        }
        user_entries.push(serde_json::json!({
            "name": name,
            "dir": udir,
            "role": role_of.get(name.as_str()).cloned().unwrap_or_else(|| "user".into()),
            "total": idxs.len(),
            "emb": has_emb,
            "months": month_list,
        }));
    }
    user_entries.sort_by_key(|u| -(u["total"].as_i64().unwrap_or(0)));

    // Categories (config): només amb fonts que existeixen de veritat.
    let known: std::collections::HashSet<&str> = user_entries
        .iter()
        .filter_map(|u| u["name"].as_str())
        .collect();
    let categories: Vec<serde_json::Value> = cfg
        .web_categories
        .iter()
        .filter_map(|(name, members)| {
            let members: Vec<&String> =
                members.iter().filter(|m| known.contains(m.as_str())).collect();
            if members.is_empty() {
                tracing::warn!(category = %name, "WEB_CATEGORIES: cap font coneguda, s'omet");
                return None;
            }
            Some(serde_json::json!({
                "name": name,
                "users": members,
                "default": cfg.web_default_category.as_deref() == Some(name.as_str()),
            }))
        })
        .collect();

    let manifest = serde_json::json!({
        "total": links.len(),
        "users": user_entries,
        "categories": categories,
    });
    std::fs::write(
        dir.join("data/manifest.json"),
        serde_json::to_string(&manifest)?,
    )?;

    let json = serde_json::to_string(&index)?;
    let json_pretty = serde_json::to_string_pretty(&index)?;
    // links.json per consum extern; links.js com a fallback per a file:// (fetch()
    // està bloquejat sota file://: app.js l'injecta si el manifest no es pot carregar).
    std::fs::write(dir.join("data/links.json"), json_pretty)?;
    std::fs::write(
        dir.join("data/links.js"),
        format!("window.__LINKS__ = {json};\n"),
    )?;

    // GitHub Pages ignora _headers (sintaxi de Cloudflare/Netlify) i serveix les
    // dades amb la seva pròpia cache-control (~10 min). Com que les dades canvien
    // entre releases, no podem confiar en {{VERSION}}: injectem un segell de
    // generació {{DATAV}} als URL de dades (?v=) perquè el navegador sempre les
    // recarregui després d'un refresc. .nojekyll evita el build de Jekyll a Pages.
    let datav = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string();
    std::fs::write(dir.join(".nojekyll"), "")?;
    std::fs::write(
        dir.join("index.html"),
        INDEX_HTML
            .replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
            .replace("{{DATAV}}", &datav),
    )?;
    std::fs::write(dir.join("css/style.css"), STYLE_CSS)?;
    std::fs::write(dir.join("js/app.js"), APP_JS.replace("{{DATAV}}", &datav))?;

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
  <meta name="app-version" content="{{VERSION}}">
  <link rel="stylesheet" href="css/style.css?v={{VERSION}}">
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
          <div class="menu-sep" id="menu-hist-sep" hidden></div>
          <button class="menu-item" data-act="follow" role="menuitem" hidden><span class="mi-ico">👥</span> Fonts que segueixes</button>
          <button class="menu-item" data-act="hist-more" role="menuitem" hidden><span class="mi-ico">▼</span> Historial: un mes més</button>
          <button class="menu-item" data-act="hist-less" role="menuitem" hidden><span class="mi-ico">▲</span> Historial: un mes menys</button>
          <div class="menu-sep"></div>
          <button class="menu-item" data-act="cols-dec" role="menuitem"><span class="mi-ico">−</span> Menys columnes</button>
          <button class="menu-item" data-act="cols-inc" role="menuitem"><span class="mi-ico">+</span> Més columnes</button>
          <div class="menu-sep"></div>
          <button class="menu-item" data-act="theme" role="menuitem"><span class="mi-ico theme-icon"></span> Canvia el tema</button>
          <div class="menu-sep"></div>
          <button class="menu-item" data-act="about" role="menuitem"><span class="mi-ico">❓</span> Què és això?</button>
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
  <script src="js/app.js?v={{VERSION}}"></script>
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
.menu-item:disabled { opacity: .4; cursor: not-allowed; }
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

/* Historial carregat (fins a quin mes es veu) */
.hist-toggle { cursor: pointer; user-select: none; transition: color var(--transition); }
.hist-toggle:hover { color: var(--fg); }
.hist-toggle.end { cursor: default; }
.hist-toggle.end:hover { color: var(--muted); }

/* Fonts seguides (chip + modal) */
.follow-toggle { cursor: pointer; user-select: none; transition: color var(--transition); }
.follow-toggle:hover { color: var(--fg); }
.flw-sec { margin: .3rem 0 .9rem; }
.flw-sec h4 { margin: 0 0 .45rem; font-size: .8rem; text-transform: uppercase; letter-spacing: .05em; color: var(--faint); }
.flw-row { display: flex; align-items: center; gap: .55rem; padding: .38rem .45rem; border-radius: 8px; cursor: pointer; }
.flw-row:hover { background: var(--card-hover); }
.flw-row input { accent-color: var(--accent); }
.flw-row .flw-n { margin-left: auto; color: var(--faint); font-size: .8rem; }
.flw-row .rolebadge { font-size: .66rem; }
.flw-foot { display: flex; gap: .5rem; justify-content: flex-end; margin-top: 1rem; padding-top: 1rem; border-top: 1px dashed var(--border); flex-wrap: wrap; }
.flw-hint { color: var(--faint); font-size: .8rem; margin-right: auto; align-self: center; }
.load-more-wrap { grid-column: 1/-1; text-align: center; padding: .4rem 0 1rem; }
.load-more {
  cursor: pointer; font-size: .88rem; padding: .55rem 1.2rem; border-radius: 10px;
  border: 1px solid var(--border); background: var(--card); color: var(--muted);
  transition: all var(--transition);
}
.load-more:hover { border-color: var(--accent); color: var(--accent); transform: translateY(-1px); }
.load-more:disabled { opacity: .5; cursor: wait; transform: none; }

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
.modal-body p { line-height: 1.55; margin: 0 0 .7rem; }
.about-ver { color: var(--faint); font-size: .85rem; margin-bottom: 0 !important; }
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

// Dades: manifest amb fonts (usuaris) + shards per font/mes/part
// (data/u/{font}/{YYYY-MM}-p{N}.json) carregats progressivament segons les
// fonts seguides. Sota file:// (fetch bloquejat) s'injecta data/links.js com a
// fallback amb tot l'índex lleuger (sense seguits ni càrrega per mesos).
const DATAV = '{{DATAV}}';
let ALL = [];            // links visibles fusionats (ordre cronològic invers)
let MANIFEST = null;     // { total, users: [{name,dir,role,total,emb,months}], categories }
let MONTHS = [];         // línia temporal fusionada de les fonts seguides: [{key,count}] desc
let SHOWN = 0;           // nombre de mesos visibles
let STATIC_MODE = false; // fallback links.js: tot carregat, sense fonts ni historial
const PART_CACHE = new Map();  // "dir|mes" -> array de links (parts fusionats)
const EXTRA = [];              // links afegits en calent via API (encara sense shard)
const EMB = new Map();         // id -> {e, s} (embedding quantitzat)
const EMB_LOADED = new Set();  // "dir|mes" amb embeddings ja carregats
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

// Mutacions locals: cal tocar també les caches perquè rebuildAll()
// reconstrueix ALL des d'allà.
function removeLocal(id) {
  ALL = ALL.filter(l => l.id !== id);
  for (const arr of PART_CACHE.values()) {
    const i = arr.findIndex(l => l.id === id);
    if (i >= 0) arr.splice(i, 1);
  }
  const x = EXTRA.findIndex(l => l.id === id);
  if (x >= 0) EXTRA.splice(x, 1);
  hearts.delete(id);
}
function addLocal(l) {
  if (ALL.some(x => x.id === l.id)) return;
  ALL.unshift(l);
  EXTRA.unshift(l);
}

async function reprocessLink(id) {
  try { await api('POST', '/links/' + id + '/reprocess'); toast('Link reencuat: es tornarà a analitzar.', 'ok'); }
  catch (e) { toast('Error en reforçar: ' + e.message, 'err'); }
}
async function deleteLink(id) {
  if (!confirm('Segur que vols donar de baixa aquest link?')) return;
  try {
    await api('DELETE', '/links/' + id);
    removeLocal(id);
    renderStats(); buildFilters(); render();
    toast('Link donat de baixa.', 'ok');
  } catch (e) { toast('Error en donar de baixa: ' + e.message, 'err'); }
}
async function blockLink(id) {
  if (!confirm('Bloquejar aquest URL? S\'afegirà a la blocklist i el link s\'esborrarà.')) return;
  try {
    await api('POST', '/links/' + id + '/block');
    removeLocal(id);
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
      try { const l = await api('GET', '/links/' + id); if (l && l.id) addLocal(l); }
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
// Hi ha embeddings publicats a alguna font seguida? Si no, el cor no té efecte
// d'ordre: l'amaguem. Els vectors viuen a data/u/{font}/emb-{mes}-p{N}.json i
// només es baixen quan hi ha cors.
function embAvailable() {
  return !STATIC_MODE && !!MANIFEST && followedUsers().some(u => u.emb);
}

// Baixa els embeddings dels mesos visibles de les fonts seguides (un fetch per
// part, un sol cop). No fa res sense cors: la majoria de visites no el paguen.
async function ensureEmb() {
  if (!embAvailable() || !hearts.size) return;
  const keys = new Set(MONTHS.slice(0, SHOWN).map(m => m.key));
  const jobs = [];
  for (const u of followedUsers()) {
    if (!u.emb) continue;
    for (const m of u.months) {
      const ck = u.dir + '|' + m.key;
      if (!keys.has(m.key) || EMB_LOADED.has(ck)) continue;
      EMB_LOADED.add(ck);
      jobs.push((async () => {
        try {
          for (let p = 0; p < (m.parts || 1); p++) {
            const d = await fetchJson('data/u/' + u.dir + '/emb-' + m.key + '-p' + p + '.json');
            for (const id in d) EMB.set(id, d[id]);
          }
        } catch (e) { EMB_LOADED.delete(ck); }
      })());
    }
  }
  await Promise.all(jobs);
}

function toggleHeart(id) {
  if (hearts.has(id)) hearts.delete(id); else hearts.add(id);
  writeHearts([...hearts]);
}

// Dequantitza l'embedding int8 d'un link -> array de floats (o null).
function embVec(id) {
  const d = EMB.get(id);
  if (!d || !Array.isArray(d.e) || typeof d.s !== 'number') return null;
  const e = d.e, s = d.s, out = new Array(e.length);
  for (let i = 0; i < e.length; i++) out[i] = e[i] * s;
  return out;
}

// Centroide (mitjana) dels embeddings dels links amb cor. null si no n'hi ha cap.
function centroid() {
  let acc = null, n = 0;
  for (const id of hearts) {
    const v = embVec(id);
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
      const r = await fetch('data/deep/' + id + '.json?v={{DATAV}}');
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
  // Sense cors, es manté l'ordre cronològic invers (created_at DESC).
  const cen = centroid();
  if (cen) items.forEach(l => { const v = embVec(l.id); l.__score = v ? cosine(v, cen) : -1; });
  if (onlyNew || cen) {
    items.sort((a, b) => {
      // Amb "novetats" actiu: nous primer, antics després; cada grup per interès.
      if (onlyNew) { const d = (isNew(b)?1:0) - (isNew(a)?1:0); if (d) return d; }
      return cen ? b.__score - a.__score : 0;
    });
  }
  renderPerso();

  // Render incremental: pintar milers de cards de cop crema CPU (i bateria als
  // mòbils). Es pinten RENDER_CAP i s'estiren més amb el botó (o en fer scroll
  // fins a ell, via IntersectionObserver).
  const visible = items.slice(0, RENDER_CAP);
  visible.forEach(l => { grid.appendChild(buildCard(l)); });

  if (!items.length) {
    const e = document.createElement('div');
    e.className = 'empty';
    e.textContent = MONTHS.length && SHOWN < MONTHS.length
      ? 'Cap resultat amb aquests filtres en el període carregat.'
      : 'Cap resultat amb aquests filtres.';
    grid.appendChild(e);
  }

  const w = document.createElement('div');
  w.className = 'load-more-wrap';
  if (items.length > visible.length) {
    // Encara hi ha links carregats per pintar.
    const b = document.createElement('button');
    b.className = 'load-more';
    b.textContent = '＋ Mostra\'n més (' + (items.length - visible.length) + ' pendents)';
    b.onclick = () => { RENDER_CAP += CAP_STEP; render(); };
    w.appendChild(b);
    grid.appendChild(w);
    observeMore(b);
  } else if (!STATIC_MODE && MONTHS.length && SHOWN < MONTHS.length) {
    // Tot el carregat és visible: oferir un mes més d'historial.
    const next = MONTHS[SHOWN];
    const b = document.createElement('button');
    b.className = 'load-more';
    b.textContent = '⬇ Carrega ' + monthLabel(next.key) + ' · ' + next.count +
      (next.count === 1 ? ' enllaç' : ' enllaços');
    b.onclick = async () => { b.disabled = true; await showMonths(SHOWN + 1); };
    w.appendChild(b);
    grid.appendChild(w);
  }
}

// Vista d'una sola card via permalink (#id:xxx). Afegeix un enllaç a l'inici.
// Si el link no és als mesos carregats, es baixa la seva fitxa data/i/{id}.json.
const SINGLE_CACHE = new Map(); // id -> link | null (null = no existeix)
function renderSingle(grid) {
  const l = ALL.find(x => x.id === activeId) || SINGLE_CACHE.get(activeId) || null;
  if (l) {
    grid.appendChild(buildCard(l));
  } else if (!STATIC_MODE && !SINGLE_CACHE.has(activeId)) {
    const id = activeId;
    grid.innerHTML = '<div class="empty">Carregant l\'enllaç…</div>';
    (async () => {
      let link = null;
      try { link = await fetchJson('data/i/' + id + '.json'); } catch (e) {}
      SINGLE_CACHE.set(id, link && link.id ? link : null);
      if (activeId === id) render();
    })();
  } else {
    grid.innerHTML = '<div class="empty">Aquest enllaç no existeix o s\'ha donat de baixa.</div>';
  }
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
          ${embAvailable() ? `<button class="heart ${hearts.has(l.id)?'on':''}" data-id="${esc(l.id)}" title="Marca per personalitzar l'ordre" aria-label="M'agrada">♥</button>` : ''}
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
    if (hb) hb.onclick = async () => { toggleHeart(l.id); await ensureEmb(); render(); };
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
  const n = [...hearts].filter(id => EMB.has(id) || ALL.some(l => l.id === id)).length;
  if (!embAvailable() || !n) { box.className = 'perso'; box.innerHTML = ''; return; }
  box.className = 'perso on';
  box.innerHTML = '<span>❤ Ordenat per afinitat amb <b>' + n + '</b> ' +
    (n === 1 ? 'enllaç marcat' : 'enllaços marcats') + '</span>' +
    '<button id="perso-clear" class="perso-clear">Neteja</button>';
  $('perso-clear').onclick = () => { hearts.clear(); writeHearts([]); render(); };
}

function renderStats() {
  const flw = (!STATIC_MODE && MANIFEST) ? followedUsers() : [];
  const total = flw.length ? flw.reduce((a, u) => a + u.total, 0) : ALL.length;
  const done = ALL.filter(l => l.status === 'done').length;
  const tags = new Set(); ALL.forEach(l => (l.tags||[]).forEach(t => tags.add(t)));
  const newCount = ALL.filter(isNew).length;
  let html =
    `<span><b>${ALL.length}</b>${total > ALL.length ? ' de ' + total : ''} enllaços</span>` +
    `<span><b>${done}</b> processats</span>` +
    `<span id="tags-toggle" class="tags-toggle${filtersOpen ? ' on' : ''}" ` +
      `title="Mostra/amaga el llistat de tags"># <b>${tags.size}</b> tags</span>`;
  // Fonts seguides; clicant s'obre el selector de fonts i categories.
  if (flw.length) {
    html += `<span id="follow-toggle" class="follow-toggle" ` +
      `title="Fonts seguides: ${flw.map(u => '@' + u.name).join(', ')}. Clica per canviar-les.">` +
      `👥 <b>${flw.length}</b> ${flw.length === 1 ? 'font' : 'fonts'}</span>`;
  }
  if (newCount) {
    html += `<span id="new-toggle" class="new-toggle${onlyNew ? ' on' : ''}" ` +
      `title="Mostra només novetats">✨ <b>${newCount}</b> novetats</span>`;
  }
  // Fins a quin mes es veu l'historial; clicant es carrega un mes més.
  if (!STATIC_MODE && MONTHS.length) {
    const cur = MONTHS[Math.min(SHOWN, MONTHS.length) - 1];
    const more = SHOWN < MONTHS.length;
    html += `<span id="hist-toggle" class="hist-toggle${more ? '' : ' end'}" ` +
      `title="${more ? 'Historial visible. Clica per carregar un mes més' : 'Tot l\'historial és visible'}">` +
      `📅 fins <b>${monthLabel(cur.key)}</b>${more ? ' ＋' : ''}</span>`;
  }
  $('stats').innerHTML = html;
  const tt = $('tags-toggle');
  if (tt) tt.onclick = () => { filtersOpen = !filtersOpen; renderStats(); buildFilters(); updateMenuState(); };
  const nt = $('new-toggle');
  if (nt) nt.onclick = () => { onlyNew = !onlyNew; renderStats(); render(); updateMenuState(); };
  const ht = $('hist-toggle');
  if (ht && SHOWN < MONTHS.length) ht.onclick = () => showMonths(SHOWN + 1);
  const ft = $('follow-toggle');
  if (ft) ft.onclick = openFollowModal;
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
    case 'follow': openFollowModal(); break;
    case 'hist-more': showMonths(SHOWN + 1); break;
    case 'hist-less': showMonths(SHOWN - 1); break;
    case 'cols-dec': colsDec(); break;
    case 'cols-inc': colsInc(); break;
    case 'theme': toggleTheme(); break;
    case 'token': promptToken(); break;
    case 'admin': openUsersModal(); break;
    case 'add': addLink(); break;
    case 'about': openAboutModal(); break;
  }
}

function openAboutModal() {
  const meta = document.querySelector('meta[name="app-version"]');
  const ver = meta ? meta.getAttribute('content') : '';
  const ov = document.createElement('div');
  ov.className = 'modal-ov';
  ov.innerHTML = `<div class="modal">
    <div class="modal-head"><h3>❓ Què és això?</h3><button class="modal-x" title="Tanca">✕</button></div>
    <div class="modal-body">
      <p><strong>Clio</strong> és un LinkAnalyzer: recull enllaços, els analitza i en genera
      resums, tipus i sentiment amb IA. Aquesta web mostra tot el que ha recollit,
      amb cerca, filtres i tags per navegar-hi.</p>
      <p class="about-ver">Versió <strong>${esc(ver)}</strong></p>
    </div>
  </div>`;
  document.body.appendChild(ov);
  const close = () => { ov.remove(); document.removeEventListener('keydown', esc2); };
  function esc2(e){ if(e.key==='Escape') close(); }
  ov.querySelector('.modal-x').onclick = close;
  ov.onclick = (e) => { if (e.target === ov) close(); };
  document.addEventListener('keydown', esc2);
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
    if (act === 'token' || act === 'admin' || act === 'add' || act === 'follow') close();
    menuAction(act);
  });
  document.addEventListener('click', (e) => { if (!menu.hidden && !menu.contains(e.target) && e.target !== btn) close(); });
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape') close(); });
  applySearch();
  refreshApiItems();
  updateMenuState();
}

// ---- Càrrega progressiva de dades (manifest + shards mensuals) ----

async function fetchJson(path) {
  const r = await fetch(path + '?v=' + DATAV);
  if (!r.ok) throw new Error('HTTP ' + r.status);
  return r.json();
}

const MONTHS_CA = ['gener','febrer','març','abril','maig','juny','juliol','agost','setembre','octubre','novembre','desembre'];
function monthLabel(key) {
  const p = (key || '').split('-');
  const m = parseInt(p[1], 10);
  return (MONTHS_CA[m - 1] || key) + ' ' + p[0];
}

// Mesos visibles persistits (0 = decideix el defecte inicial).
function readMonthsCookie() { const m = document.cookie.match(/(?:^|;\s*)clio_months=(\d+)/); return m ? parseInt(m[1], 10) : 0; }
function writeMonthsCookie(n) { document.cookie = 'clio_months=' + n + '; path=/; max-age=31536000; SameSite=Lax'; }

// ---- Fonts seguides (cookie clio_follow: {u:[usuaris], c:[categories]}) ----
// null = defecte: la categoria marcada com a default al manifest, o totes les fonts.
function readFollow() {
  const m = document.cookie.match(/(?:^|;\s*)clio_follow=([^;]*)/);
  if (!m) return null;
  try {
    const f = JSON.parse(decodeURIComponent(m[1]));
    return (f && (Array.isArray(f.u) || Array.isArray(f.c))) ? { u: f.u || [], c: f.c || [] } : null;
  } catch (e) { return null; }
}
function writeFollow(f) {
  document.cookie = f
    ? 'clio_follow=' + encodeURIComponent(JSON.stringify(f)) + '; path=/; max-age=31536000; SameSite=Lax'
    : 'clio_follow=; path=/; max-age=0; SameSite=Lax';
}
let FOLLOW = readFollow();

function catByName(n) { return ((MANIFEST && MANIFEST.categories) || []).find(c => c.name === n); }

// Noms de les fonts seguides, resolts contra el manifest. Mai buit si hi ha
// fonts: una selecció que ja no existeix cau al defecte (i el defecte, a tot).
function followedNames() {
  if (!MANIFEST) return new Set();
  const all = MANIFEST.users.map(u => u.name);
  let sel = new Set();
  if (FOLLOW) {
    (FOLLOW.u || []).forEach(n => sel.add(n));
    (FOLLOW.c || []).forEach(cn => { const c = catByName(cn); if (c) c.users.forEach(n => sel.add(n)); });
    sel = new Set([...sel].filter(n => all.includes(n)));
  }
  if (!sel.size) {
    const def = (MANIFEST.categories || []).find(c => c.default);
    (def ? def.users : all).forEach(n => sel.add(n));
    sel = new Set([...sel].filter(n => all.includes(n)));
    if (!sel.size) all.forEach(n => sel.add(n));
  }
  return sel;
}
function followedUsers() {
  if (!MANIFEST) return [];
  const s = followedNames();
  return MANIFEST.users.filter(u => s.has(u.name));
}

// Línia temporal fusionada (mesos desc) de les fonts seguides.
function computeMonths() {
  const agg = new Map();
  for (const u of followedUsers()) {
    for (const m of u.months) agg.set(m.key, (agg.get(m.key) || 0) + m.count);
  }
  MONTHS = [...agg.entries()].map(([key, count]) => ({ key, count }))
    .sort((a, b) => (a.key < b.key ? 1 : -1));
}

// Carrega totes les parts d'un mes per a cada font seguida que el tingui.
async function loadMonth(key) {
  const jobs = [];
  for (const u of followedUsers()) {
    const m = u.months.find(x => x.key === key);
    if (!m) continue;
    const ck = u.dir + '|' + key;
    if (PART_CACHE.has(ck)) continue;
    PART_CACHE.set(ck, []);
    jobs.push((async () => {
      try {
        const parts = await Promise.all(Array.from({ length: m.parts || 1 },
          (_, p) => fetchJson('data/u/' + u.dir + '/' + key + '-p' + p + '.json')));
        PART_CACHE.set(ck, [].concat.apply([], parts));
      } catch (e) { PART_CACHE.delete(ck); }
    })());
  }
  await Promise.all(jobs);
}

// Reconstrueix ALL amb els SHOWN primers mesos de les fonts seguides.
// Dedup per id: un link co-reportat apareix al shard de cada reporter.
function rebuildAll() {
  const seen = new Set();
  ALL = [];
  for (const l of EXTRA) { if (!seen.has(l.id)) { seen.add(l.id); ALL.push(l); } }
  const keys = MONTHS.slice(0, SHOWN).map(m => m.key);
  for (const u of followedUsers()) {
    for (const k of keys) {
      const arr = PART_CACHE.get(u.dir + '|' + k) || [];
      for (const l of arr) { if (!seen.has(l.id)) { seen.add(l.id); ALL.push(l); } }
    }
  }
  ALL.sort((a, b) => linkTime(b) - linkTime(a));
}

// Fixa el nombre de mesos visibles (clamp a [1, total]), carregant el que falti.
async function showMonths(n, persist = true) {
  if (STATIC_MODE || !MONTHS.length) return;
  const grow = n > SHOWN;
  n = Math.max(1, Math.min(n, MONTHS.length));
  SHOWN = n;
  if (persist) writeMonthsCookie(n);
  await Promise.all(MONTHS.slice(0, n).map(m => loadMonth(m.key)));
  rebuildAll();
  await ensureEmb();
  // En estirar historial, deixar marge de render perquè el nou mes es vegi.
  if (grow) RENDER_CAP += CAP_STEP;
  refreshHistItems();
  renderStats(); buildFilters(); render();
}

// Re-aplica un canvi de fonts seguides: recalcula mesos i recarrega el que calgui.
async function applyFollow() {
  computeMonths();
  resetCap();
  const n = SHOWN || initialMonths();
  SHOWN = 0; // força showMonths encara que n no canviï
  await showMonths(n, false);
}

// Mesos inicials: cookie si n'hi ha; si no, els que calguin per a ~60 enllaços.
function initialMonths() {
  const c = readMonthsCookie();
  if (c) return c;
  let acc = 0, n = 0;
  for (const m of MONTHS) { n++; acc += m.count; if (acc >= 60) break; }
  return Math.max(n, 1);
}

// Ítems del menú de fonts/historial: només amb manifest (i >1 mes per l'historial).
function refreshHistItems() {
  const fol = document.querySelector('.menu-item[data-act="follow"]');
  const more = document.querySelector('.menu-item[data-act="hist-more"]');
  const less = document.querySelector('.menu-item[data-act="hist-less"]');
  const sep = $('menu-hist-sep');
  const base = !STATIC_MODE && !!MANIFEST;
  const ok = base && MONTHS.length > 1;
  if (fol) fol.hidden = !base;
  if (sep) sep.hidden = !base;
  if (more) { more.hidden = !ok; more.disabled = ok && SHOWN >= MONTHS.length; }
  if (less) { less.hidden = !ok; less.disabled = ok && SHOWN <= 1; }
}

// ---- Modal de fonts i categories ----
function openFollowModal() {
  if (STATIC_MODE || !MANIFEST) return;
  const cats = MANIFEST.categories || [];
  const selC = new Set(FOLLOW ? (FOLLOW.c || []) : cats.filter(c => c.default).map(c => c.name));
  const selU = new Set(FOLLOW ? (FOLLOW.u || []) : []);
  const catTotal = (c) => c.users.reduce((a, n) => {
    const u = MANIFEST.users.find(x => x.name === n);
    return a + (u ? u.total : 0);
  }, 0);

  const ov = document.createElement('div');
  ov.className = 'modal-ov';
  const catRows = cats.map(c =>
    `<label class="flw-row"><input type="checkbox" data-cat="${esc(c.name)}"${selC.has(c.name) ? ' checked' : ''}>
      <span>${esc(c.name)}${c.default ? ' <span class="you">(defecte)</span>' : ''}</span>
      <span class="flw-n" title="${esc(c.users.join(', '))}">${c.users.length} fonts · ${catTotal(c)}</span></label>`).join('');
  const userRows = MANIFEST.users.map(u =>
    `<label class="flw-row"><input type="checkbox" data-user="${esc(u.name)}"${selU.has(u.name) ? ' checked' : ''}>
      <span>@${esc(u.name)}</span>${u.role === 'npc' ? ' <span class="rolebadge user">npc</span>' : ''}
      <span class="flw-n">${u.total}</span></label>`).join('');
  ov.innerHTML = `<div class="modal">
    <div class="modal-head"><h3>👥 Fonts que segueixes</h3><button class="modal-x" title="Tanca">✕</button></div>
    <div class="modal-body">
      ${cats.length ? `<div class="flw-sec"><h4>Categories</h4>${catRows}</div>` : ''}
      <div class="flw-sec"><h4>Fonts</h4>${userRows}</div>
      <div class="flw-foot">
        <span class="flw-hint">Sense selecció es mostra ${cats.some(c => c.default) ? 'la categoria per defecte' : 'tot'}.</span>
        <button id="flw-reset" class="act">Per defecte</button>
        <button id="flw-save" class="act">✓ Desa</button>
      </div>
    </div>
  </div>`;
  document.body.appendChild(ov);
  const close = () => { ov.remove(); document.removeEventListener('keydown', esc3); };
  function esc3(e) { if (e.key === 'Escape') close(); }
  ov.querySelector('.modal-x').onclick = close;
  ov.onclick = (e) => { if (e.target === ov) close(); };
  document.addEventListener('keydown', esc3);

  ov.querySelector('#flw-reset').onclick = async () => {
    FOLLOW = null; writeFollow(null);
    close(); await applyFollow();
    toast('Fonts restaurades al defecte.', 'ok');
  };
  ov.querySelector('#flw-save').onclick = async () => {
    const u = [...ov.querySelectorAll('input[data-user]:checked')].map(i => i.dataset.user);
    const c = [...ov.querySelectorAll('input[data-cat]:checked')].map(i => i.dataset.cat);
    FOLLOW = (u.length || c.length) ? { u, c } : null;
    writeFollow(FOLLOW);
    close(); await applyFollow();
    toast('Fonts actualitzades: ' + followedUsers().length + ' seguides.', 'ok');
  };
}

// ---- Render incremental ----
// Pintar tot el que hi ha carregat de cop és el que crema CPU al mòbil: es
// pinta a trams de CAP_STEP i s'estira amb el botó o fent scroll fins a ell.
const CAP_STEP = 60;
let RENDER_CAP = CAP_STEP;
function resetCap() { RENDER_CAP = CAP_STEP; }
let moreObserver = null;
function observeMore(btn) {
  if (!('IntersectionObserver' in window)) return;
  if (!moreObserver) {
    moreObserver = new IntersectionObserver(entries => {
      for (const en of entries) {
        if (en.isIntersecting) { moreObserver.unobserve(en.target); en.target.click(); }
      }
    }, { rootMargin: '600px' });
  }
  moreObserver.observe(btn);
}

// Fallback file:// (o manifest absent): injecta data/links.js amb tot l'índex
// lleuger incrustat. Sense embeddings ni historial per mesos.
function loadFallbackScript() {
  return new Promise(resolve => {
    const s = document.createElement('script');
    s.src = 'data/links.js?v=' + DATAV;
    s.onload = () => resolve(true);
    s.onerror = () => resolve(false);
    document.head.appendChild(s);
  });
}

async function loadData() {
  if (location.protocol !== 'file:') {
    try {
      const m = await fetchJson('data/manifest.json');
      if (m && Array.isArray(m.users)) { MANIFEST = m; return; }
    } catch (e) {}
  }
  STATIC_MODE = true;
  await loadFallbackScript();
  ALL = Array.isArray(window.__LINKS__) ? window.__LINKS__ : [];
  ALL.sort((a, b) => linkTime(b) - linkTime(a));
}

(async function init() {
  initTheme();
  initCols();
  await probeApi();
  await loadMe();
  initMenu();
  await loadData();
  applyHash();
  if (MANIFEST) {
    computeMonths();
    await showMonths(initialMonths(), false);
  } else {
    renderStats();
    buildFilters();
    render();
  }
  refreshHistItems();
  window.addEventListener('hashchange', () => { resetCap(); onHashChange(); });
  $('search').addEventListener('input', () => { resetCap(); render(); });
  $('type-filter').addEventListener('change', () => { resetCap(); render(); });
  $('sent-filter').addEventListener('change', () => { resetCap(); render(); });
  // Marca aquesta visita: els links nous deixaran de ser-ho a la pròxima.
  writeLastVisit(Date.now());
})();
"##;
