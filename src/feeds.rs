//! Col·lectors NPC: usuaris automàtics (`users.role = 'npc'`) que recullen
//! enllaços de fonts externes i els reporten pel MATEIX camí que qualsevol
//! usuari (`AppState::report_link`), de manera que hereten co-reporting,
//! dedup i pipeline sense codi nou.
//!
//! Fase actual: RSS/Atom. `scrape` (pàgina → notícies via IA) arribarà després.

use crate::error::{AppError, Result};
use crate::models::{Feed, FeedKind};
use crate::service::AppState;
use chrono::Utc;

/// Màxim d'entrades processades per col·lecta: evita que un feed prolífic
/// inundi el ranking en un sol tick.
const MAX_ITEMS_PER_RUN: usize = 25;

/// Granularitat del scheduler: cada quant revisa si algun feed ha vençut.
/// El període real de cada feed el marca `interval_s`.
const POLL_SECS: u64 = 60;

/// Bucle de fons (spawn a `serve`): cada `POLL_SECS` col·lecta els feeds
/// habilitats que han vençut (`now - last_run >= interval_s`).
pub async fn run(state: AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(POLL_SECS));
    loop {
        tick.tick().await;
        let feeds = match state.db.enabled_feeds().await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "feeds: no s'han pogut llegir");
                continue;
            }
        };
        let now = Utc::now();
        for feed in feeds {
            let due = match feed.last_run {
                Some(t) => (now - t).num_seconds() >= feed.interval_s,
                None => true,
            };
            if !due {
                continue;
            }
            if let Err(e) = collect(&state, &feed).await {
                tracing::warn!(feed = %feed.source, error = %e, "feed: col·lecta fallida");
            }
            if let Err(e) = state.db.touch_feed(feed.id).await {
                tracing::warn!(error = %e, "feed: touch fallit");
            }
        }
    }
}

async fn collect(state: &AppState, feed: &Feed) -> Result<()> {
    match feed.kind {
        FeedKind::Rss => collect_rss(state, feed).await,
        FeedKind::Scrape => {
            tracing::info!(feed = %feed.source, "scrape encara no implementat");
            Ok(())
        }
    }
}

async fn collect_rss(state: &AppState, feed: &Feed) -> Result<()> {
    let npc = state
        .db
        .user_by_id(feed.user_id)
        .await?
        .ok_or_else(|| AppError::Pipeline(format!("feed {}: NPC {} no existeix", feed.id, feed.user_id)))?;

    let bytes = state
        .http
        .get(&feed.source)
        .send()
        .await
        .map_err(|e| AppError::Pipeline(format!("fetch RSS: {e}")))?
        .bytes()
        .await
        .map_err(|e| AppError::Pipeline(format!("cos RSS: {e}")))?;

    let parsed = feed_rs::parser::parse(&bytes[..])
        .map_err(|e| AppError::Pipeline(format!("parse RSS: {e}")))?;

    let mut new = 0usize;
    for entry in parsed.entries.into_iter().take(MAX_ITEMS_PER_RUN) {
        let Some(url) = entry.links.into_iter().map(|l| l.href).next() else {
            continue;
        };
        match state.report_link(&npc, &url).await {
            Ok(outcome) => {
                if outcome.needs_processing {
                    state.enqueue(outcome.link_id);
                }
                if outcome.is_new {
                    new += 1;
                }
            }
            Err(e) => tracing::warn!(%url, error = %e, "feed: report fallit"),
        }
    }
    tracing::info!(feed = %feed.source, npc = %npc.username, new, "feed RSS col·lectat");
    Ok(())
}
