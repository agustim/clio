use crate::error::{AppError, Result};
use crate::models::UserRole;
use crate::service::AppState;
use crate::{api, telegram, webgen};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "linkanalyzer", about = "Clio · recull, analitza i publica enllaços")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Arrenca el servidor API (i el bot si està configurat)
    Serve,
    /// Crea un usuari i mostra el seu api_token
    UserAdd {
        username: String,
        #[arg(long)]
        admin: bool,
    },
    /// Afegeix un link i el processa (síncron)
    Add {
        url: String,
        /// api_token; si s'omet usa l'usuari local 'cli'
        #[arg(long)]
        token: Option<String>,
    },
    /// Llista els darrers links
    List {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Genera la web estàtica a ./public
    Generate,
    /// Genera embeddings per als links que en manquin (backfill, requereix LLM)
    Reindex,
    /// Reprocessa links existents (re-analitza => títols curts nous, tags, etc.)
    Reprocess {
        #[arg(long, default_value_t = 1000)]
        limit: i64,
        /// Només la primera passada (no clona repos ni re-fa l'anàlisi profunda)
        #[arg(long)]
        shallow: bool,
    },
    /// Dona de baixa un link pel seu id
    Delete {
        id: String,
    },
    /// Commit + push de la web (opt-in, requereix WEB_REPO_URL)
    Push,
    /// Crea un NPC (usuari col·lector automàtic) i mostra el seu api_token
    NpcAdd {
        username: String,
    },
    /// Afegeix un feed RSS/Atom a un NPC
    FeedAdd {
        /// username de l'NPC propietari
        npc: String,
        /// URL del feed RSS/Atom
        url: String,
        /// Període mínim entre col·lectes, en segons
        #[arg(long, default_value_t = 3600)]
        interval: i64,
    },
    /// Llista els feeds configurats
    FeedList,
}

pub async fn run(
    state: AppState,
    rx: tokio::sync::mpsc::Receiver<crate::queue::Job>,
    cmd: Cmd,
) -> Result<()> {
    match cmd {
        Cmd::Serve => serve(state, rx).await,
        Cmd::UserAdd { username, admin } => {
            let role = if admin { UserRole::Admin } else { UserRole::User };
            let user = state.db.create_user(&username, role).await?;
            println!("Usuari creat: {} ({})", user.username, user.role);
            println!("API token:    {}", user.api_token);
            Ok(())
        }
        Cmd::Add { url, token } => {
            let user = match token {
                Some(t) => state
                    .db
                    .user_by_token(&t)
                    .await?
                    .ok_or(AppError::Unauthorized)?,
                None => state.db.ensure_cli_user().await?,
            };
            let outcome = state.report_link(&user, &url).await?;
            println!(
                "Link {} ({})",
                outcome.link_id,
                if outcome.is_new { "nou" } else { "ja existia" }
            );
            // En CLI processem ara (síncron, shallow + deep) per veure el resultat.
            if outcome.needs_processing {
                print!("Processant (shallow + deep)… ");
                if let Err(e) = state.process_full(outcome.link_id).await {
                    println!("error: {e}");
                } else {
                    println!("fet.");
                }
            }
            if let Some(l) = state.db.link_by_id(outcome.link_id).await? {
                println!("Estat:     {} (deep: {})", l.status, l.deep_status);
                println!("  títol:     {}", l.title.clone().unwrap_or_default());
                println!("  tipus:     {}", l.link_type);
                println!("  sentiment: {}", l.sentiment);
                println!("  tags:      {}", l.tags.join(", "));
                println!("  reporters: {}", l.reporter_count());
                if let Some(ds) = &l.deep_summary {
                    let preview: String = ds.chars().take(200).collect();
                    println!("  deep:      {preview}");
                }
                if let Some(cs) = &l.code_stats {
                    println!("  codi:      {cs}");
                }
            }
            Ok(())
        }
        Cmd::List { limit } => {
            let links = state.db.list_links(None, None, None, limit).await?;
            if links.is_empty() {
                println!("(cap link)");
            }
            for l in links {
                println!(
                    "[{}] {:<8} 👥{} {}",
                    l.status,
                    l.link_type.to_string(),
                    l.reporter_count(),
                    l.title.unwrap_or_else(|| l.url.clone())
                );
                println!("        {}", l.url);
            }
            Ok(())
        }
        Cmd::Generate => {
            webgen::generate(&state.db, &state.cfg).await?;
            println!("Web generada a ./{}", state.cfg.public_dir);
            Ok(())
        }
        Cmd::Reindex => {
            if state.embedder.is_none() {
                println!("Embeddings no configurats (EMBED_PROVIDER): res a fer.");
                return Ok(());
            }
            let (done, total) = state.reindex_embeddings().await?;
            println!("Embeddings generats: {done}/{total}");
            Ok(())
        }
        Cmd::Reprocess { limit, shallow } => {
            let ids = state.db.all_link_ids(limit).await?;
            let total = ids.len();
            println!("Reprocessant {total} links ({})…", if shallow { "shallow" } else { "shallow + deep" });
            let (mut ok, mut err) = (0usize, 0usize);
            for (i, id) in ids.into_iter().enumerate() {
                let res = if shallow {
                    let llm = state.llm.as_deref();
                    let embedder = state.embedder.as_deref();
                    crate::pipeline::process_link(&state.db, &state.cfg, &state.http, llm, embedder, id).await
                } else {
                    state.process_full(id).await
                };
                match res {
                    Ok(_) => {
                        ok += 1;
                        if let Some(l) = state.db.link_by_id(id).await? {
                            println!("  [{}/{}] {}", i + 1, total, l.title.unwrap_or_else(|| l.url.clone()));
                        }
                    }
                    Err(e) => { err += 1; println!("  [{}/{}] error: {e}", i + 1, total); }
                }
            }
            println!("Fet: {ok} ok, {err} errors. Regenera la web amb `generate`.");
            Ok(())
        }
        Cmd::Delete { id } => {
            let uuid = uuid::Uuid::parse_str(&id)
                .map_err(|_| AppError::BadRequest("id invàlid".into()))?;
            let deleted = state.db.delete_link(uuid).await?;
            println!("{}", if deleted { "Link esborrat." } else { "No existeix cap link amb aquest id." });
            Ok(())
        }
        Cmd::Push => {
            webgen::generate(&state.db, &state.cfg).await?;
            webgen::git_push(&state.cfg, "chore: update static web")?;
            println!("Push completat (o omès si no configurat).");
            Ok(())
        }
        Cmd::NpcAdd { username } => {
            let user = state.db.create_user(&username, UserRole::Npc).await?;
            println!("NPC creat: {} ({})", user.username, user.id);
            println!("API token: {}", user.api_token);
            Ok(())
        }
        Cmd::FeedAdd { npc, url, interval } => {
            let user = state
                .db
                .user_by_username(&npc)
                .await?
                .ok_or_else(|| AppError::BadRequest(format!("NPC '{npc}' no existeix")))?;
            let feed = state
                .db
                .create_feed(user.id, crate::models::FeedKind::Rss, &url, interval)
                .await?;
            println!(
                "Feed RSS afegit a @{}: {} (cada {}s)",
                npc, feed.source, feed.interval_s
            );
            Ok(())
        }
        Cmd::FeedList => {
            let feeds = state.db.list_feeds().await?;
            if feeds.is_empty() {
                println!("(cap feed)");
            }
            for f in feeds {
                let owner = state
                    .db
                    .user_by_id(f.user_id)
                    .await?
                    .map(|u| u.username)
                    .unwrap_or_default();
                println!(
                    "[{}] {} @{} cada {}s  {}",
                    f.kind,
                    if f.enabled { "on" } else { "off" },
                    owner,
                    f.interval_s,
                    f.source
                );
            }
            Ok(())
        }
    }
}

async fn serve(state: AppState, rx: tokio::sync::mpsc::Receiver<crate::queue::Job>) -> Result<()> {
    let addr = state.cfg.bind_addr.clone();
    let workers = state.cfg.queue_workers;

    // Workers de la cua d'anàlisi.
    let q_state = state.clone();
    tokio::spawn(async move { crate::queue::run(q_state, rx, workers).await });

    // Recovery: re-encua feina pendent de la DB.
    state.recover().await?;

    // Genera la web un cop a l'arrencada (perquè / mostri alguna cosa).
    if let Err(e) = webgen::generate(&state.db, &state.cfg).await {
        tracing::warn!(error = %e, "generació inicial de web fallida");
    }

    // Regeneració periòdica perquè la web reflecteixi els nous links.
    let regen_secs = state.cfg.web_regen_secs;
    if regen_secs > 0 {
        let regen_state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(regen_secs));
            tick.tick().await; // descarta el primer (immediat); ja s'ha generat a l'arrencada
            loop {
                tick.tick().await;
                if let Err(e) = webgen::generate(&regen_state.db, &regen_state.cfg).await {
                    tracing::warn!(error = %e, "regeneració de web fallida");
                }
            }
        });
    }

    // Deploy reactiu: en completar-se l'anàlisi d'un link, regenera + push.
    // El debounce agrupa una ràfega de links nous en un sol deploy. git_push
    // fa no-op si links.json no ha canviat, així que CF Pages només reconstrueix
    // quan hi ha contingut nou de veritat.
    let deploy_state = state.clone();
    let debounce = std::time::Duration::from_secs(state.cfg.web_debounce_secs);
    tokio::spawn(async move {
        loop {
            deploy_state.web_dirty.notified().await;
            tokio::time::sleep(debounce).await;
            if let Err(e) = webgen::generate(&deploy_state.db, &deploy_state.cfg).await {
                tracing::warn!(error = %e, "regen per deploy fallida");
                continue;
            }
            if let Err(e) = webgen::git_push(&deploy_state.cfg, "chore: update links") {
                tracing::warn!(error = %e, "push de deploy fallit");
            }
        }
    });

    // Bot en background (stub).
    let bot_state = state.clone();
    tokio::spawn(async move { telegram::run(bot_state).await });

    // Col·lectors NPC (RSS ara; scrape més endavant).
    let feeds_state = state.clone();
    tokio::spawn(async move { crate::feeds::run(feeds_state).await });

    let public_dir = state.cfg.public_dir.clone();
    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| AppError::Config(format!("bind {addr}: {e}")))?;
    tracing::info!("API a http://{addr}/api/v1 · web a http://{addr}/ (dir: {public_dir})");
    axum::serve(listener, app)
        .await
        .map_err(|e| AppError::Config(format!("server: {e}")))?;
    Ok(())
}
