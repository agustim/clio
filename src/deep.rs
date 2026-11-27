use crate::config::Config;
use crate::db::Db;
use crate::error::{AppError, Result};
use crate::llm::LlmClient;
use crate::models::{Link, LinkType};
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
    // Job duplicat (reaper + cua endarrerida) o reprocessament concurrent: si
    // ja hi ha anàlisi feta no tornem a clonar el repo, que pot trigar minuts i
    // bloqueja un dels quatre workers. `process_full`/`retry_link` posen
    // l'estat a `pending` abans de cridar-nos, així que això no impedeix
    // refer l'anàlisi a propòsit.
    if link.deep_status == crate::models::DeepStatus::Done {
        tracing::debug!(%link_id, "deep ja fet, s'omet");
        return Ok(());
    }
    db.set_deep_status(link_id, crate::models::DeepStatus::Processing).await?;

    let result = match link.link_type {
        LinkType::Repo => match forge_repo(&link.url) {
            // Subpàgina (issue, PR, blob, tree, releases...): no és clonable.
            Some(f) if !f.is_root => deep_repo_page(cfg, http, llm, &link.url, &f.root).await,
            // Arrel (o URL d'un forge que no sabem descompondre): clone normal.
            Some(f) => deep_repo(cfg, llm, &f.root).await,
            None => deep_repo(cfg, llm, &link.url).await,
        },
        LinkType::Article | LinkType::Blog | LinkType::News => {
            deep_article(cfg, http, llm, &link.url).await.map(|s| (s, None))
        }
        LinkType::Video => deep_video(cfg, llm, &link.url).await,
        // Tuits de X/Twitter: llegim la fitxa pública (fxtwitter) i n'analitzem
        // el contingut. La resta de socials no tenen deep.
        LinkType::Social if crate::social::is_twitter_url(&link.url) => {
            deep_tweet(db, cfg, http, llm, &link).await
        }
        _ => Ok(("(no aplica)".to_string(), None)),
    };

    match result {
        Ok((summary, stats)) => {
            // Veu de la notícia (best-effort): titular + resum de 40-50 paraules
            // (TTS_SCRIPT_WORDS). Es fa ABANS de marcar el deep com a `done`
            // perquè la card no aparegui a l'overlay sense veu; si el LLM o la
            // sintesi fallen, el deep continua igualment.
            crate::voice::prepare(db, cfg, llm, &link, &summary).await;
            db.update_deep_analysis(link_id, &summary, stats.as_ref()).await?;
            tracing::info!(%link_id, "deep pass done");
            Ok(())
        }
        Err(e) => {
            if let crate::error::AppError::Llm(_) = &e {
                tracing::debug!(%link_id, error = %e, "deep pass failed (afecta al LLM)");
            } else {
                tracing::warn!(%link_id, error = %e, "deep pass failed");
            }
            db.set_deep_status(link_id, crate::models::DeepStatus::Failed).await?;
            Err(e)
        }
    }
}

// ---------- REPOS ----------

/// Forges on una URL pot ser l'arrel d'un repositori clonable.
const FORGES: &[&str] = &["github.com", "gitlab.com", "bitbucket.org"];

/// URL d'un forge descomposta en arrel de repositori.
struct ForgeRepo {
    /// URL canònica de l'arrel (`https://github.com/owner/repo`), sense query
    /// ni barra final: és el que `git clone` accepta.
    root: String,
    /// Cert si la URL rebuda ja era l'arrel (no una subpàgina).
    is_root: bool,
}

/// Detecta si `url` és d'un forge conegut i en calcula l'arrel.
///
/// `git clone` només accepta l'arrel del repositori: amb
/// `.../issues/123` o `.../blob/main/x.md` respon «repository not found».
/// Retorna `None` si no és cap forge o si no té prou segments (`owner/repo`).
///
/// Nota: als grups niats de GitLab (`grup/subgrup/repo`) no es pot saber
/// l'arrel sense l'API; s'agafen els dos primers segments i, si el clone
/// falla, el deep de pàgina segueix funcionant igualment.
fn forge_repo(url: &str) -> Option<ForgeRepo> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(host_end);
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.to_ascii_lowercase();
    let host = host.trim_start_matches("www.");
    if !FORGES.contains(&host) {
        return None;
    }
    let path = tail.split(['?', '#']).next().unwrap_or("");
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 || segs[0] == "-" {
        return None;
    }
    Some(ForgeRepo {
        root: format!("{scheme}://{host}/{}/{}", segs[0], segs[1]),
        is_root: segs.len() == 2,
    })
}

/// Mètriques d'un repositori clonat.
struct RepoInfo {
    scan: CodeScan,
    readme: Option<String>,
    bytes: u64,
}

/// Clona `root` (blobless + sparse) i n'extreu les mètriques.
async fn clone_info(cfg: &Config, root: &str) -> Result<RepoInfo> {
    let tmp = std::env::temp_dir().join(format!("clio-clone-{}", Uuid::new_v4()));
    let guard = TmpGuard(tmp.clone());

    clone_repo(cfg, root, &tmp).await?;

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
    drop(guard); // esborra el tmp
    Ok(RepoInfo { scan, readme, bytes })
}

fn repo_stats_json(info: &RepoInfo) -> Value {
    json!({
        "files": info.scan.files,
        "loc": info.scan.loc,
        "bytes": info.bytes,
        "languages": info.scan.languages_json(),
        "top_languages": info.scan.top_languages(5),
        "has_readme": info.readme.is_some(),
    })
}

/// Clona el repo (depth 1, sense submodules), escaneja el codi i en fa anàlisi.
async fn deep_repo(
    cfg: &Config,
    llm: Option<&LlmClient>,
    url: &str,
) -> Result<(String, Option<Value>)> {
    let info = clone_info(cfg, url).await?;

    let summary = match llm {
        Some(client) => {
            let prompt = repo_prompt(url, &info.scan, info.readme.as_deref(), cfg.summary_max_words);
            // Fallada del LLM => es propaga (el deep queda en 'failed', reintentable
            // amb «Refer») en lloc de publicar el README original, que podria ser
            // en un altre idioma. `complete` ja reintenta errors transitoris.
            let s = client.complete(&prompt).await?;
            pipeline::polish_summary(&s)
        }
        None => repo_fallback(&info.scan, info.readme.as_deref()),
    };

    Ok((summary, Some(repo_stats_json(&info))))
}

/// Deep d'una URL que apunta a una **subpàgina** d'un repositori: issue, pull
/// request, discussió, `blob/`, `tree/`, `releases/`, `commit/`, wiki...
///
/// Aquestes URL no són clonables, però el seu contingut sí que és a la pàgina
/// HTML (GitHub/GitLab la renderitzen al servidor). Per tant la font principal
/// és la pàgina; el clon de l'arrel és només un enriquiment (mètriques del
/// projecte) i, si falla, no invalida el resum.
async fn deep_repo_page(
    cfg: &Config,
    http: &reqwest::Client,
    llm: Option<&LlmClient>,
    page_url: &str,
    root: &str,
) -> Result<(String, Option<Value>)> {
    let page = pipeline::fetch(http, cfg, page_url).await;
    let parsed = match &page {
        Ok(html) => Some(pipeline::parse(html)),
        Err(e) => {
            tracing::warn!(%page_url, error = %e, "deep repo page: pàgina no llegible");
            None
        }
    };
    let cloned = clone_info(cfg, root).await;
    if let Err(e) = &cloned {
        tracing::info!(%root, error = %e, "deep repo page: sense clon de l'arrel, seguim amb la pàgina");
    }

    // Sense pàgina i sense clon no hi ha res a analitzar: és una fallada real.
    if parsed.is_none() && cloned.is_err() {
        return Err(AppError::Pipeline(format!(
            "ni pàgina ni repositori: {}",
            page.err().map(|e| e.to_string()).unwrap_or_default()
        )));
    }

    let title = parsed
        .as_ref()
        .and_then(|p| p.title.clone())
        .unwrap_or_default();
    let text: String = parsed
        .as_ref()
        .map(|p| p.text.chars().take(16000).collect())
        .unwrap_or_default();

    let summary = match llm {
        Some(client) if !text.trim().is_empty() => {
            let prompt = repo_page_prompt(
                page_url,
                root,
                &title,
                &text,
                cloned.as_ref().ok(),
                cfg.summary_max_words,
            );
            // Fallada del LLM => es propaga (deep 'failed', reintentable amb
            // «Refer»), igual que a la resta de passades profundes.
            let s = client.complete(&prompt).await?;
            pipeline::polish_summary(&s)
        }
        _ if !text.trim().is_empty() => article_fallback(&text, cfg.summary_max_words),
        // Sense text de pàgina però amb clon: resum del repositori sencer.
        _ => match (&cloned, llm) {
            (Ok(info), Some(client)) => {
                let prompt = repo_prompt(root, &info.scan, info.readme.as_deref(), cfg.summary_max_words);
                let s = client.complete(&prompt).await?;
                pipeline::polish_summary(&s)
            }
            (Ok(info), None) => repo_fallback(&info.scan, info.readme.as_deref()),
            _ => return Err(AppError::Pipeline("deep repo page: sense contingut".into())),
        },
    };

    // Si el clon ha fallat no posem stats a zero: la fitxa de la web mostraria
    // «0 fitxers / 0 LOC» per a una pàgina que sí que té contingut. Sense stats
    // la fitxa simplement no surt i el resum de la pàgina es publica igualment.
    let stats = cloned.as_ref().ok().map(|info| {
        let mut stats = repo_stats_json(info);
        if let Some(obj) = stats.as_object_mut() {
            obj.insert("kind".into(), json!("repo_page"));
            obj.insert("repo".into(), json!(root));
            obj.insert("page_url".into(), json!(page_url));
            obj.insert("page_available".into(), json!(parsed.is_some()));
        }
        stats
    });
    Ok((summary, stats))
}

/// Prompt per a una subpàgina de repositori: context del projecte + contingut.
fn repo_page_prompt(
    page_url: &str,
    root: &str,
    title: &str,
    text: &str,
    info: Option<&RepoInfo>,
    max_words: usize,
) -> String {
    let ctx = match info {
        Some(i) => {
            let langs: Vec<String> = i
                .scan
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
                "REPOSITORI: {root}\nFITXERS DE CODI: {}, LÍNIES TOTALS: {}, LLENGUATGES: {}",
                i.scan.files,
                i.scan.loc,
                if langs.is_empty() { "desconeguts".to_string() } else { langs.join(", ") }
            )
        }
        None => format!("REPOSITORI: {root}"),
    };
    format!(
        "Ets un enginyer de programari. Aquesta URL és una pàgina DINS d'un repositori \
         (issue, pull request, discussió, fitxer o release), no l'arrel del repositori.\n\
         Escriu EN CATALÀ una anàlisi de menys de {max_words} paraules: de què tracta la \
         pàgina, quin problema o canvi descriu i què en destaca. Si el text és d'un fitxer, \
         explica'n el contingut.\n\
         IMPORTANT: comença directament pel contingut — no obris amb «Aquesta pàgina...», \
         «El repositori...» ni frases metalingüístiques similars — i sé fidel al text: no \
         inventis fets, xifres ni característiques que no hi surtin.\n\n\
         PÀGINA: {page_url}\n{ctx}\nTÍTOL: {title}\n\n\
         CONTINGUT DE LA PÀGINA (retallat):\n{text}"
    )
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
    let out = run_child_timeout(cfg, &mut cmd, "git").await?;
    if !out.status.success() {
        return Err(AppError::Pipeline(format!(
            "git failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Executa un procés i espera'n la sortida amb timeout.
///
/// CRÍTIC: quan el timeout expira s'ha de matar el procés i, sobretot, ESPERAR-LO
/// (`wait`, és a dir `waitpid`) per reaparellar-lo. Un fill que no es reaparella
/// es queda com a zombie; l'acumulació de zombies exhaureix el límit de
/// processos del contenidor i aleshores qualsevol intent de crear un fil o procés
/// nou falla amb EAGAIN ("Resource temporarily unavailable (os error 11)"),
/// inclosos els workers del pool de SQLite — que és l'origen dels errors
/// "database error: error communicating with database" i els avisos de Telegram
/// "⚠️ Anàlisi fallida". (Abans es feia `cmd.output()` dins de `timeout(...)`:
/// el Child es descartava sense matar ni reaparellar.)
async fn run_child_timeout(
    cfg: &Config,
    cmd: &mut tokio::process::Command,
    what: &str,
) -> Result<std::process::Output> {
    use tokio::io::AsyncReadExt;
    let timeout = Duration::from_secs(cfg.clone_timeout_secs);

    let mut child = cmd
        // IMPORTANT: pipes de stdout/stderr (abans `cmd.output()` les posava
        // implícitament). Sense això `child.stdout.take()`/`stderr.take()` donen
        // None, es perd la sortida i el git/yt-dlp escriu directament als logs
        // del contenidor (stale output, errors sense detall).
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Pipeline(format!("{what} spawn: {e}")))?;

    // Traiem les pipes de stdout/stderr per poder-les llegir mentre el procés
    // corre, i mantenim el Child accessible per poder-lo matar/reaparellar a
    // l'arms del timeout.
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();

    let wait = async {
        let read_out = async {
            if let Some(s) = so.as_mut() {
                let _ = s.read_to_end(&mut stdout).await;
            }
        };
        let read_err = async {
            if let Some(s) = se.as_mut() {
                let _ = s.read_to_end(&mut stderr).await;
            }
        };
        tokio::join!(read_out, read_err);
        child.wait().await
    };

    match tokio::time::timeout(timeout, wait).await {
        Ok(Ok(status)) => Ok(std::process::Output { status, stdout, stderr }),
        Ok(Err(e)) => Err(AppError::Pipeline(format!("{what} wait: {e}"))),
        Err(_) => {
            // Timeout: matem el procés i (crucialment) l'esperem per NO deixar
            // cap zombie al contenidor.
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
            Err(AppError::Pipeline(format!("{what}: timeout")))
        }
    }
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
        "Ets un enginyer de programari. Analitza aquest repositori i escriu EN CATALÀ \
         una anàlisi tècnica de menys de {max_words} paraules: de què va, tecnologies, \
         arquitectura probable i punts destacables.\n\
         IMPORTANT: comença directament pel contingut — no obris mai amb «Aquest repositori \
         descriu...» ni similars — i sé fidel al codi i al README: no inventis tecnologies ni \
         característiques que no hi surtin.\n\n\
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

// ---------- TUITS (X/Twitter) ----------

/// Deep d'un tuit: recupera la fitxa pública (text, autor, mètriques, media)
/// via fxtwitter, la desa a `code_stats` i en fa una anàlisi en català.
///
/// La fitxa es desa ABANS de cridar l'LLM: si el model falla, la web ja pot
/// mostrar autor/mètriques/text i el deep queda `failed` per reintentar-se.
async fn deep_tweet(
    db: &Db,
    cfg: &Config,
    http: &reqwest::Client,
    llm: Option<&LlmClient>,
    link: &Link,
) -> Result<(String, Option<Value>)> {
    let Some(info) = crate::social::fetch_tweet_info(http, &link.url).await else {
        // Twitt esborrat/protegit o API caiguda: no deixem el deep en `failed`
        // per sempre (es reintentaria en bucle); conservem el resum ja publicat
        // i la fitxa prèvia (si n'hi havia) en lloc d'esborrar-la.
        tracing::info!(url = %link.url, "deep tweet: sense dades públiques del twitt");
        return Ok((existing_tweet_summary(link), link.code_stats.clone()));
    };

    let stats = tweet_stats(&info);
    if let Err(e) = db.set_deep_stats(link.id, &stats).await {
        tracing::warn!(link_id = %link.id, error = %e, "deep tweet: no s'han pogut desar les stats");
    }
    // Imatge d'acompanyament (foto o miniatura del vídeo) si encara no en tenia:
    // així el deep també arregla els tuits antics sense imatge.
    if link.image_url.is_none() {
        if let Some(img) = info.image() {
            match db.set_link_image(link.id, Some(&img)).await {
                Ok(()) => {
                    // Còpia local (data/images) perquè la web estàtica la pugui
                    // servir sense dependre del proxy /img.
                    if let Err(e) = pipeline::cache_link_image(db, cfg, http, link.id, &img).await {
                        tracing::warn!(link_id = %link.id, error = %e, "deep tweet: còpia local d'imatge fallida");
                    }
                }
                Err(e) => tracing::warn!(link_id = %link.id, error = %e, "deep tweet: no s'ha pogut desar la imatge"),
            }
        }
    }

    let text: String = info.text.trim().chars().take(16000).collect();
    if text.is_empty() {
        tracing::info!(url = %link.url, "deep tweet: text buit (només media)");
        return Ok((existing_tweet_summary(link), Some(stats)));
    }

    let summary = match llm {
        Some(client) => {
            let prompt = tweet_prompt(&info, &text, cfg.summary_max_words * 2);
            // Fallada del LLM => es propaga (deep `failed`, reintentable) en lloc
            // de publicar el text original en un altre idioma.
            let s = client.complete(&prompt).await?;
            pipeline::polish_summary(&s)
        }
        None => article_fallback(&text, cfg.summary_max_words * 2),
    };
    Ok((summary, Some(stats)))
}

/// Resum de reserva quan no hi ha contingut nou del twitt.
fn existing_tweet_summary(link: &Link) -> String {
    link.summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(no aplica)")
        .to_string()
}

/// Fitxa estructurada del twitt per a `code_stats` (la llegeix la web i,
/// potencialment, l'overlay).
fn tweet_stats(info: &crate::social::TweetInfo) -> Value {
    let media: Vec<Value> = info
        .media
        .iter()
        .map(|m| json!({ "kind": m.kind, "url": m.url, "thumbnail_url": m.thumbnail_url }))
        .collect();
    json!({
        "kind": "tweet",
        "author": {
            "name": info.author_name,
            "screen_name": info.author_screen,
            "url": info.author_url,
            "avatar_url": info.avatar_url,
            "followers": info.followers,
        },
        "created_at": info.created_at,
        "created_timestamp": info.created_timestamp,
        "likes": info.likes,
        "retweets": info.retweets,
        "replies": info.replies,
        "quotes": info.quotes,
        "bookmarks": info.bookmarks,
        "views": info.views,
        "lang": info.lang,
        "media": media,
        "text": info.text.chars().take(2000).collect::<String>(),
    })
}

/// Prompt de l'anàlisi profunda d'un tuit (en català, fidel al text).
fn tweet_prompt(info: &crate::social::TweetInfo, text: &str, max_words: usize) -> String {
    let author = match (info.author_name.as_str(), info.author_screen.as_str()) {
        ("", "") => "un usuari de X".to_string(),
        ("", h) => format!("@{h}"),
        (n, "") => n.to_string(),
        (n, h) => format!("{n} (@{h})"),
    };
    let media_desc = if info.media.iter().any(|m| m.kind != "photo") {
        " Acompanya el tuit amb mitjans audiovisuals (vídeo/GIF); no els pots veure: basat només en el text."
    } else if !info.media.is_empty() {
        " Acompanya el tuit amb imatges; no les pots veure: basat només en el text."
    } else {
        ""
    };
    format!(
        "Ets un analista. Aquest és un tuit de X/Twitter de {author}.{media_desc}\n\
         Escriu EN CATALÀ una anàlisi en prosa periodística (context, idees clau i \
         conclusions) en menys de {max_words} paraules.\n\
         IMPORTANT: comença directament pel contingut — no obris amb «Aquest tuit parla de...», \
         «L'autor explica...» ni frases metalingüístiques similars. Sé fidel al text: no inventis \
         fets, xifres ni opinions que no hi siguin.\n\n{text}"
    )
}

// ---------- ARTICLES ----------

/// Re-descarrega el text complet (sense truncar a 4000) i en fa un resum llarg.
async fn deep_article(
    cfg: &Config,
    http: &reqwest::Client,
    llm: Option<&LlmClient>,
    url: &str,
) -> Result<String> {
    let html = pipeline::fetch(http, cfg, url).await?;
    let parsed = pipeline::parse(&html);
    let full: String = parsed.text.chars().take(16000).collect();

    let summary = match llm {
        Some(client) => {
            let prompt = format!(
                "Escriu EN CATALÀ un resum en prosa periodística d'aquest article (anàlisi en \
                 profunditat, punts clau i conclusions) en menys de {} paraules.\n\
                 IMPORTANT: el primer text que escriguis ha de ser directament la notícia o el \
                 tema — no obris mai amb frases que presentin l'article com «L'article \
                 descriu...», «Aquest article analitza...», «L'anàlisi de l'article...» ni \
                 similars. Sé fidel al contingut: no inventis fets, xifres, cites ni opinions \
                 que no surtin al text.\n\n{}",
                cfg.summary_max_words * 2,
                full
            );
            // Fallada del LLM => es propaga (el deep queda en 'failed', reintentable
            // amb «Refer») en lloc d'editar el text original de l'article, que
            // seria en la llengua de la font.
            let s = client.complete(&prompt).await?;
            pipeline::polish_summary(&s)
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
                 seva {} següent, escriu EN CATALÀ un resum en prosa periodística (punts clau \
                 i conclusions) en menys de {} paraules.\n\
                 IMPORTANT: comença directament pel contingut — no obris amb frases \
                 metalingüístiques com «El vídeo descriu...», «Aquest vídeo tracta...», \
                 «L'anàlisi...» ni similars. Sé fidel al contingut: no afegeixis fets ni dades \
                 que no hi surtin.\n\n{body}",
                if transcript.is_empty() { "descripció" } else { "transcripció" },
                cfg.summary_max_words * 2,
            );
            // Fallada del LLM => es propaga (el deep queda en 'failed', reintentable
            // amb «Refer») en lloc de publicar la transcripció/descripció original.
            let s = client.complete(&prompt).await?;
            pipeline::polish_summary(&s)
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
    let out = run_child_timeout(cfg, &mut cmd, "yt-dlp").await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DeepStatus, LinkStatus, Sentiment};
    use crate::social::{MediaItem, TweetInfo};

    fn info() -> TweetInfo {
        TweetInfo {
            text: "Opus 5 va escriure 5500 línies de codi per renderitzar el Senyor dels Anells.".into(),
            author_name: "Andrej Karpathy".into(),
            author_screen: "karpathy".into(),
            author_url: Some("https://x.com/karpathy".into()),
            avatar_url: Some("https://pbs.twimg.com/a.jpg".into()),
            followers: 4_470_090,
            created_at: Some("2026-08-02T03:00:09Z".into()),
            created_timestamp: Some(1_785_639_609),
            likes: 29_790,
            retweets: 2_311,
            replies: 1_803,
            quotes: 753,
            bookmarks: 12_127,
            views: 6_323_136,
            lang: "en".into(),
            media: vec![MediaItem {
                kind: "video".into(),
                url: "https://video.twimg.com/v.mp4".into(),
                thumbnail_url: Some("https://pbs.twimg.com/t.jpg".into()),
            }],
        }
    }

    fn link(summary: Option<&str>) -> Link {
        let now = chrono::Utc::now();
        Link {
            id: Uuid::new_v4(),
            url: "https://x.com/karpathy/status/1".into(),
            title: Some("t".into()),
            summary: summary.map(String::from),
            link_type: LinkType::Social,
            tags: Vec::new(),
            sentiment: Sentiment::Neutral,
            status: LinkStatus::Done,
            co_reporters: Vec::new(),
            reporters: Vec::new(),
            deep_status: DeepStatus::Processing,
            deep_summary: None,
            code_stats: None,
            image_url: None,
            image_file: None,
            audio_file: None,
            audio_script: None,
            embedding: None,
            embed_scale: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn tweet_stats_carry_author_metrics_media_and_text() {
        let s = tweet_stats(&info());
        assert_eq!(s["kind"], "tweet");
        assert_eq!(s["author"]["screen_name"], "karpathy");
        assert_eq!(s["author"]["followers"].as_u64(), Some(4_470_090));
        assert_eq!(s["likes"].as_u64(), Some(29_790));
        assert_eq!(s["views"].as_u64(), Some(6_323_136));
        assert_eq!(s["created_timestamp"].as_i64(), Some(1_785_639_609));
        assert_eq!(s["media"][0]["kind"], "video");
        assert_eq!(s["media"][0]["thumbnail_url"], "https://pbs.twimg.com/t.jpg");
        assert!(s["text"].as_str().unwrap().contains("5500 línies"));
        // El text gran es retalla (no infla l'índex de la web).
        let mut big = info();
        big.text = "a".repeat(5000);
        assert_eq!(tweet_stats(&big)["text"].as_str().unwrap().chars().count(), 2000);
    }

    #[test]
    fn tweet_prompt_is_catalan_and_faithful() {
        let p = tweet_prompt(&info(), "text del tuit", 400);
        assert!(p.contains("EN CATALÀ"));
        assert!(p.contains("Andrej Karpathy (@karpathy)"));
        assert!(p.contains("no inventis"));
        assert!(p.contains("text del tuit"));
        assert!(p.contains("400"));
    }

    #[test]
    fn existing_summary_falls_back_to_existing_or_placeholder() {
        assert_eq!(existing_tweet_summary(&link(Some("resum curt"))), "resum curt");
        assert_eq!(existing_tweet_summary(&link(Some("   "))), "(no aplica)");
        assert_eq!(existing_tweet_summary(&link(None)), "(no aplica)");
    }

    #[test]
    fn forge_root_of_a_repo_root() {
        let f = forge_repo("https://github.com/astral-sh/uv").unwrap();
        assert!(f.is_root);
        assert_eq!(f.root, "https://github.com/astral-sh/uv");
    }

    #[test]
    fn forge_root_strips_query_and_trailing_slash() {
        let f = forge_repo("https://github.com/astral-sh/uv/?tab=readme-ov-file").unwrap();
        assert!(f.is_root);
        assert_eq!(f.root, "https://github.com/astral-sh/uv");
    }

    #[test]
    fn forge_root_of_subpages() {
        // Les URL que abans es passaven a `git clone` i fallaven.
        for (url, root) in [
            ("https://github.com/astral-sh/uv/pull/21327", "https://github.com/astral-sh/uv"),
            ("https://github.com/zedeus/nitter/issues/1442/", "https://github.com/zedeus/nitter"),
            ("https://github.com/immich-app/immich/discussions/29439", "https://github.com/immich-app/immich"),
            ("https://github.com/audacity/audacity/releases/tag/Audacity-4.0.0", "https://github.com/audacity/audacity"),
            ("https://github.com/openai/math/blob/main/preprints/x/paper.pdf", "https://github.com/openai/math"),
            ("https://github.com/humanlayer/x/tree/main/docs", "https://github.com/humanlayer/x"),
        ] {
            let f = forge_repo(url).unwrap();
            assert!(!f.is_root, "{url} hauria de ser subpàgina");
            assert_eq!(f.root, root);
        }
    }

    #[test]
    fn forge_root_gitlab_and_bitbucket() {
        let g = forge_repo("https://gitlab.com/owner/repo/-/issues/7").unwrap();
        assert!(!g.is_root);
        assert_eq!(g.root, "https://gitlab.com/owner/repo");

        let b = forge_repo("https://bitbucket.org/owner/repo/src/main/README.md").unwrap();
        assert!(!b.is_root);
        assert_eq!(b.root, "https://bitbucket.org/owner/repo");
    }

    #[test]
    fn forge_root_normalizes_host_and_www() {
        let f = forge_repo("HTTPS://WWW.GitHub.com/Owner/Repo").unwrap();
        assert!(f.is_root);
        assert_eq!(f.root, "https://github.com/Owner/Repo");
    }

    #[test]
    fn forge_root_ignores_non_forges_and_short_paths() {
        // "github.com" dins d'una URL de web.archive.org no és un repo.
        assert!(forge_repo("https://web.archive.org/web/2026/https://github.com/x/y").is_none());
        assert!(forge_repo("https://github.com/owner").is_none());
        assert!(forge_repo("https://example.com/owner/repo/issues/1").is_none());
    }

    #[test]
    fn repo_page_prompt_marks_it_as_a_subpage_and_is_faithful() {
        let p = repo_page_prompt(
            "https://github.com/o/r/pull/1",
            "https://github.com/o/r",
            "Fix the parser",
            "text de la pàgina",
            None,
            400,
        );
        assert!(p.contains("EN CATALÀ"));
        assert!(p.contains("DINS d'un repositori"));
        assert!(p.contains("Fix the parser"));
        assert!(p.contains("text de la pàgina"));
        assert!(p.contains("no inventis"));
        assert!(p.contains("400"));
    }
}
