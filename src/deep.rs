use crate::config::Config;
use crate::db::Db;
use crate::error::{AppError, Result};
use crate::llm::LlmClient;
use crate::models::LinkType;
use crate::pipeline;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// Punt d'entrada de la segona passada. Decideix segons el tipus de link.
pub async fn process_deep(
    db: &Db,
    cfg: &Config,
    http: &reqwest::Client,
    llm: Option<&LlmClient>,
    link_id: Uuid,
) -> Result<()> {
    let link = db.link_by_id(link_id).await?.ok_or(AppError::NotFound)?;
    db.set_deep_status(link_id, crate::models::DeepStatus::Processing).await?;

    let result = match link.link_type {
        LinkType::Repo => deep_repo(cfg, llm, &link.url).await,
        LinkType::Article | LinkType::Blog | LinkType::News => {
            deep_article(cfg, http, llm, &link.url).await.map(|s| (s, None))
        }
        LinkType::Video => deep_video(cfg, llm, &link.url).await,
        _ => Ok(("(no aplica)".to_string(), None)),
    };

    match result {
        Ok((summary, stats)) => {
            db.update_deep_analysis(link_id, &summary, stats.as_ref()).await?;
            tracing::info!(%link_id, "deep pass done");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(%link_id, error = %e, "deep pass failed");
            db.set_deep_status(link_id, crate::models::DeepStatus::Failed).await?;
            Err(e)
        }
    }
}

// ---------- REPOS ----------

/// Clona el repo (depth 1, sense submodules), escaneja el codi i en fa anàlisi.
async fn deep_repo(
    cfg: &Config,
    llm: Option<&LlmClient>,
    url: &str,
) -> Result<(String, Option<Value>)> {
    let tmp = std::env::temp_dir().join(format!("clio-clone-{}", Uuid::new_v4()));
    let guard = TmpGuard(tmp.clone());

    clone_repo(cfg, url, &tmp).await?;

    // Límit de mida després del clone.
    let bytes = dir_size(&tmp).unwrap_or(0);
    let max = cfg.clone_max_mb * 1024 * 1024;
    if bytes > max {
        return Err(AppError::Pipeline(format!(
            "repo massa gran: {} MB > {} MB",
            bytes / 1024 / 1024,
            cfg.clone_max_mb
        )));
    }

    let scan = scan_code(&tmp);
    let readme = read_readme(&tmp);

    let stats = json!({
        "files": scan.files,
        "loc": scan.loc,
        "bytes": bytes,
        "languages": scan.languages_json(),
        "top_languages": scan.top_languages(5),
        "has_readme": readme.is_some(),
    });

    let summary = match llm {
        Some(client) => {
            let prompt = repo_prompt(url, &scan, readme.as_deref(), cfg.summary_max_words);
            match client.complete(&prompt).await {
                Ok(s) => s.trim().to_string(),
                Err(e) => {
                    tracing::warn!(error = %e, "llm deep repo failed, fallback");
                    repo_fallback(&scan, readme.as_deref())
                }
            }
        }
        None => repo_fallback(&scan, readme.as_deref()),
    };

    drop(guard); // esborra el tmp
    Ok((summary, Some(stats)))
}

/// Globs de fitxers que ens interessen al checkout sparse (codi + readme).
/// Evita materialitzar binaris grans (datasets, models, vídeos) que disparen
/// el límit de mida i no aporten res a l'anàlisi.
const SPARSE_GLOBS: &[&str] = &[
    "*.rs", "*.py", "*.js", "*.mjs", "*.cjs", "*.ts", "*.tsx", "*.jsx", "*.go",
    "*.java", "*.kt", "*.c", "*.h", "*.cpp", "*.cc", "*.hpp", "*.cxx", "*.cs",
    "*.rb", "*.php", "*.swift", "*.scala", "*.sh", "*.bash", "*.html", "*.css",
    "*.scss", "*.sass", "*.sql", "*.md", "*.toml", "*.yaml", "*.yml", "*.json",
    "README*", "readme*", "LICENSE*",
];

/// Clona el repo en mode parcial (blobless) i sparse: només es baixen els blobs
/// dels fitxers de codi/readme, no els binaris grans. Així repos enormes (per
/// assets) caben sota `clone_max_mb`.
async fn clone_repo(cfg: &Config, url: &str, dest: &Path) -> Result<()> {
    if !url.starts_with("https://") {
        return Err(AppError::Pipeline("clone: només https".into()));
    }

    // 1. Clone blobless, sparse, sense checkout encara.
    run_git(
        cfg,
        &[
            "clone",
            "--depth",
            "1",
            "--no-recurse-submodules",
            "--filter=blob:none",
            "--sparse",
            "--no-checkout",
            "--quiet",
            url,
            dest.to_str().unwrap_or_default(),
        ],
    )
    .await?;

    // 2. Defineix el patró sparse (no-cone, estil gitignore = inclou).
    let mut sparse_args: Vec<&str> = vec!["-C", dest.to_str().unwrap_or_default(), "sparse-checkout", "set", "--no-cone"];
    sparse_args.extend_from_slice(SPARSE_GLOBS);
    run_git(cfg, &sparse_args).await?;

    // 3. Checkout: baixa només els blobs dels fitxers inclosos.
    run_git(cfg, &["-C", dest.to_str().unwrap_or_default(), "checkout", "--quiet"]).await?;
    Ok(())
}

/// Executa `git` amb timeout, prompt de credencials desactivat i LFS saltat.
async fn run_git(cfg: &Config, args: &[&str]) -> Result<()> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1");
    let fut = cmd.output();
    let out = tokio::time::timeout(Duration::from_secs(cfg.clone_timeout_secs), fut)
        .await
        .map_err(|_| AppError::Pipeline("git: timeout".into()))?
        .map_err(|e| AppError::Pipeline(format!("git spawn: {e}")))?;
    if !out.status.success() {
        return Err(AppError::Pipeline(format!(
            "git failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[derive(Default)]
struct CodeScan {
    files: u64,
    loc: u64,
    /// extensió -> (fitxers, línies)
    langs: HashMap<String, (u64, u64)>,
}

impl CodeScan {
    fn top_languages(&self, n: usize) -> Vec<Value> {
        let mut v: Vec<(&String, &(u64, u64))> = self.langs.iter().collect();
        v.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
        v.into_iter()
            .take(n)
            .map(|(lang, (files, loc))| json!({ "lang": lang, "files": files, "loc": loc }))
            .collect()
    }
    fn languages_json(&self) -> Value {
        let map: serde_json::Map<String, Value> = self
            .langs
            .iter()
            .map(|(k, (f, l))| (k.clone(), json!({ "files": f, "loc": l })))
            .collect();
        Value::Object(map)
    }
}

const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "dist", "build", "vendor", ".venv", "__pycache__"];

fn ext_to_lang(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "Rust",
        "py" => "Python",
        "js" | "mjs" | "cjs" => "JavaScript",
        "ts" | "tsx" => "TypeScript",
        "jsx" => "JSX",
        "go" => "Go",
        "java" => "Java",
        "kt" => "Kotlin",
        "c" | "h" => "C",
        "cpp" | "cc" | "hpp" | "cxx" => "C++",
        "cs" => "C#",
        "rb" => "Ruby",
        "php" => "PHP",
        "swift" => "Swift",
        "scala" => "Scala",
        "sh" | "bash" => "Shell",
        "html" => "HTML",
        "css" | "scss" | "sass" => "CSS",
        "sql" => "SQL",
        "md" => "Markdown",
        "toml" | "yaml" | "yml" | "json" => "Config",
        _ => return None,
    })
}

fn scan_code(root: &Path) -> CodeScan {
    let mut scan = CodeScan::default();
    walk(root, &mut scan, 0);
    scan
}

fn walk(dir: &Path, scan: &mut CodeScan, depth: usize) {
    if depth > 12 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(&path, scan, depth + 1);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(lang) = ext_to_lang(&ext.to_lowercase()) {
                let loc = count_lines(&path).unwrap_or(0);
                scan.files += 1;
                scan.loc += loc;
                let e = scan.langs.entry(lang.to_string()).or_insert((0, 0));
                e.0 += 1;
                e.1 += loc;
            }
        }
    }
}

fn count_lines(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 2 * 1024 * 1024 {
        return Some(0); // ignora fitxers enormes (minificats, etc.)
    }
    let content = std::fs::read_to_string(path).ok()?;
    Some(content.lines().count() as u64)
}

fn read_readme(root: &Path) -> Option<String> {
    for name in ["README.md", "README.MD", "readme.md", "README", "README.txt"] {
        let p = root.join(name);
        if let Ok(s) = std::fs::read_to_string(&p) {
            return Some(s.chars().take(4000).collect());
        }
    }
    None
}

fn repo_prompt(url: &str, scan: &CodeScan, readme: Option<&str>, max_words: usize) -> String {
    let langs: Vec<String> = scan
        .top_languages(6)
        .iter()
        .filter_map(|v| {
            Some(format!(
                "{} ({} LOC)",
                v.get("lang")?.as_str()?,
                v.get("loc")?.as_u64()?
            ))
        })
        .collect();
    format!(
        "Ets un enginyer de programari. Analitza aquest repositori i escriu en CATALÀ \
         una anàlisi tècnica de menys de {max_words} paraules: de què va, tecnologies, \
         arquitectura probable i punts destacables.\n\n\
         REPO: {url}\n\
         FITXERS DE CODI: {files}, LÍNIES TOTALS: {loc}\n\
         LLENGUATGES: {langs}\n\n\
         README (retallat):\n{readme}",
        files = scan.files,
        loc = scan.loc,
        langs = langs.join(", "),
        readme = readme.unwrap_or("(sense README)"),
    )
}

fn repo_fallback(scan: &CodeScan, readme: Option<&str>) -> String {
    let langs: Vec<String> = scan
        .top_languages(5)
        .iter()
        .filter_map(|v| Some(v.get("lang")?.as_str()?.to_string()))
        .collect();
    let mut s = format!(
        "Repositori amb {} fitxers de codi i {} línies. Llenguatges principals: {}.",
        scan.files,
        scan.loc,
        if langs.is_empty() { "desconeguts".into() } else { langs.join(", ") }
    );
    if let Some(r) = readme {
        let intro: String = r.lines().take(5).collect::<Vec<_>>().join(" ");
        if !intro.trim().is_empty() {
            s.push_str("\n\nREADME: ");
            s.push_str(intro.trim());
        }
    }
    s
}

// ---------- ARTICLES ----------

/// Re-descarrega el text complet (sense truncar a 4000) i en fa un resum llarg.
async fn deep_article(
    cfg: &Config,
    http: &reqwest::Client,
    llm: Option<&LlmClient>,
    url: &str,
) -> Result<String> {
    let html = pipeline::fetch(http, url, cfg.max_link_size_bytes).await?;
    let parsed = pipeline::parse(&html);
    let full: String = parsed.text.chars().take(16000).collect();

    let summary = match llm {
        Some(client) => {
            let prompt = format!(
                "Resumeix en CATALÀ aquest article de manera detallada (anàlisi en profunditat, \
                 punts clau i conclusions) en menys de {} paraules:\n\n{}",
                cfg.summary_max_words * 2,
                full
            );
            match client.complete(&prompt).await {
                Ok(s) => s.trim().to_string(),
                Err(e) => {
                    tracing::warn!(error = %e, "llm deep article failed, fallback");
                    article_fallback(&parsed.text, cfg.summary_max_words * 2)
                }
            }
        }
        None => article_fallback(&parsed.text, cfg.summary_max_words * 2),
    };
    Ok(summary)
}

// ---------- VÍDEOS ----------

/// Deep d'un vídeo (YouTube/Vimeo): extreu metadades + transcripció amb yt-dlp
/// i en fa un resum amb el LLM. Retorna també `code_stats`-style amb durada/canal.
async fn deep_video(
    cfg: &Config,
    llm: Option<&LlmClient>,
    url: &str,
) -> Result<(String, Option<Value>)> {
    let meta = ytdlp_meta(cfg, url).await?;
    let transcript = ytdlp_transcript(cfg, url).await.unwrap_or_default();

    let title = meta.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let channel = meta
        .get("channel")
        .or_else(|| meta.get("uploader"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let description = meta.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let duration = meta.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;

    // Material per al LLM: transcripció si n'hi ha, si no la descripció.
    let body: String = if !transcript.is_empty() {
        transcript.chars().take(16000).collect()
    } else {
        description.chars().take(8000).collect()
    };

    let summary = match llm {
        Some(client) if !body.trim().is_empty() => {
            let prompt = format!(
                "Aquest és un vídeo titulat «{title}» del canal «{channel}». A partir de la \
                 seva {} següent, fes un resum detallat en CATALÀ (punts clau i conclusions) \
                 en menys de {} paraules:\n\n{body}",
                if transcript.is_empty() { "descripció" } else { "transcripció" },
                cfg.summary_max_words * 2,
            );
            match client.complete(&prompt).await {
                Ok(s) => s.trim().to_string(),
                Err(e) => {
                    tracing::warn!(error = %e, "llm deep video failed, fallback");
                    article_fallback(&body, cfg.summary_max_words * 2)
                }
            }
        }
        _ => article_fallback(&body, cfg.summary_max_words * 2),
    };

    let stats = json!({
        "channel": channel,
        "duration_secs": duration,
        "has_transcript": !transcript.is_empty(),
    });
    Ok((summary, Some(stats)))
}

/// Crida yt-dlp `--dump-single-json` per a metadades (sense baixar el vídeo).
async fn ytdlp_meta(cfg: &Config, url: &str) -> Result<Value> {
    let out = run_ytdlp(cfg, &["--dump-single-json", "--no-warnings", "--skip-download", url]).await?;
    serde_json::from_slice(&out).map_err(|e| AppError::Pipeline(format!("yt-dlp json: {e}")))
}

/// Baixa els subtítols (auto o manuals) i en retorna el text pla.
async fn ytdlp_transcript(cfg: &Config, url: &str) -> Result<String> {
    let dir = std::env::temp_dir().join(format!("clio-yt-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).ok();
    let guard = TmpGuard(dir.clone());
    let out_tpl = dir.join("sub.%(ext)s");

    run_ytdlp(
        cfg,
        &[
            "--skip-download",
            "--write-auto-subs",
            "--write-subs",
            "--sub-langs",
            "ca,es,en",
            "--sub-format",
            "vtt",
            "--no-warnings",
            "-o",
            out_tpl.to_str().unwrap_or("sub"),
            url,
        ],
    )
    .await?;

    // Agafa el primer .vtt generat i el converteix a text pla.
    let entry = std::fs::read_dir(&dir)
        .ok()
        .and_then(|mut e| e.find_map(|x| x.ok().map(|x| x.path()).filter(|p| {
            p.extension().and_then(|x| x.to_str()) == Some("vtt")
        })));
    let path = entry.ok_or_else(|| AppError::Pipeline("sense subtítols".into()))?;
    let raw = std::fs::read_to_string(&path).map_err(|e| AppError::Pipeline(format!("read sub: {e}")))?;
    drop(guard);
    Ok(vtt_to_text(&raw))
}

/// Executa yt-dlp amb timeout i retorna l'stdout.
async fn run_ytdlp(cfg: &Config, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = tokio::process::Command::new("yt-dlp");
    cmd.args(args);
    let fut = cmd.output();
    let out = tokio::time::timeout(Duration::from_secs(cfg.clone_timeout_secs), fut)
        .await
        .map_err(|_| AppError::Pipeline("yt-dlp: timeout".into()))?
        .map_err(|e| AppError::Pipeline(format!("yt-dlp spawn: {e}")))?;
    if !out.status.success() {
        return Err(AppError::Pipeline(format!(
            "yt-dlp failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// Converteix un VTT/SRT a text pla: treu timestamps, tags i línies repetides.
fn vtt_to_text(raw: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut last = String::new();
    for line in raw.lines() {
        let l = line.trim();
        if l.is_empty()
            || l == "WEBVTT"
            || l.starts_with("Kind:")
            || l.starts_with("Language:")
            || l.contains("-->")
            || l.chars().all(|c| c.is_ascii_digit())
        {
            continue;
        }
        // Treu tags inline tipus <00:00:01.000> o <c>.
        let clean = strip_tags(l);
        let clean = clean.trim();
        if clean.is_empty() || clean == last {
            continue;
        }
        last = clean.to_string();
        out.push(clean.to_string());
    }
    out.join(" ")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn article_fallback(text: &str, max_words: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() > max_words {
        words[..max_words].join(" ")
    } else {
        words.join(" ")
    }
}

// ---------- util ----------

fn dir_size(path: &Path) -> Option<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        for entry in std::fs::read_dir(&p).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(m) = entry.metadata() {
                total += m.len();
            }
        }
    }
    Some(total)
}

/// Esborra el directori temporal en sortir d'abast.
struct TmpGuard(PathBuf);
impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
