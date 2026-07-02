mod api;
mod cli;
mod config;
mod db;
mod deep;
mod embed;
mod error;
mod feeds;
mod flaresolverr;
mod llm;
mod models;
mod normalize;
mod pipeline;
mod queue;
mod service;
mod social;
mod telegram;
mod webgen;

use clap::Parser;
use config::Config;
use db::Db;
use service::AppState;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    if let Err(e) = run().await {
        tracing::error!("fatal: {e}");
        std::process::exit(1);
    }
}

async fn run() -> error::Result<()> {
    let args = cli::Cli::parse();
    let cfg = Config::from_env()?;
    let db = Db::connect(&cfg.database_url).await?;
    let (state, rx) = AppState::new(db, cfg)?;
    cli::run(state, rx, args.command).await
}
