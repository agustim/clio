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
    /// Commit + push de la web (opt-in, requereix WEB_REPO_URL)
    Push,
}

pub async fn run(state: AppState, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Serve => serve(state).await,
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
            // En CLI processem ara (síncron) per veure el resultat.
            if outcome.needs_processing {
                print!("Processant… ");
                if let Err(e) = state.process_now(outcome.link_id).await {
                    println!("error: {e}");
                }
            }
            if let Some(l) = state.db.link_by_id(outcome.link_id).await? {
                println!("Estat: {}", l.status);
                println!("  títol:     {}", l.title.clone().unwrap_or_default());
                println!("  tipus:     {}", l.link_type);
                println!("  sentiment: {}", l.sentiment);
                println!("  tags:      {}", l.tags.join(", "));
                println!("  reporters: {}", l.reporter_count());
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
        Cmd::Push => {
            webgen::generate(&state.db, &state.cfg).await?;
            webgen::git_push(&state.cfg, "chore: update static web")?;
            println!("Push completat (o omès si no configurat).");
            Ok(())
        }
    }
}

async fn serve(state: AppState) -> Result<()> {
    let addr = state.cfg.bind_addr.clone();

    // Bot en background (stub).
    let bot_state = state.clone();
    tokio::spawn(async move { telegram::run(bot_state).await });

    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| AppError::Config(format!("bind {addr}: {e}")))?;
    tracing::info!("API escoltant a http://{addr}");
    axum::serve(listener, app)
        .await
        .map_err(|e| AppError::Config(format!("server: {e}")))?;
    Ok(())
}
