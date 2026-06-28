use crate::config::Config;
use crate::db::Db;
use crate::error::Result;
use crate::llm::LlmClient;
use crate::models::{LinkStatus, User};
use crate::normalize::normalize_url;
use crate::pipeline;
use std::sync::Arc;
use uuid::Uuid;

/// Estat compartit per API, CLI i Bot.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
    pub llm: Option<Arc<LlmClient>>,
}

pub struct ReportOutcome {
    pub link_id: Uuid,
    pub is_new: bool,
    pub added_reporter: bool,
    /// El link cal (re)processar-lo. El caller decideix com (spawn vs síncron).
    pub needs_processing: bool,
}

impl AppState {
    pub fn new(db: Db, cfg: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let llm = pipeline::build_llm(&cfg, http.clone());
        Ok(Self { db, cfg: Arc::new(cfg), http, llm })
    }

    /// Logica de recepció + deduplicació + co-reporting.
    /// NO processa: retorna `needs_processing` perquè el caller decideixi
    /// (API => spawn background, CLI => síncron).
    pub async fn report_link(&self, user: &User, raw_url: &str) -> Result<ReportOutcome> {
        let url = normalize_url(raw_url)?;

        if let Some(existing) = self.db.link_by_url(&url).await? {
            // Co-report: afegeix reporter + report.
            let added = self.db.add_co_reporter(existing.id, user.id).await?;
            self.db.add_report(existing.id, user.id).await?;

            let needs = matches!(existing.status, LinkStatus::Pending | LinkStatus::Failed);
            return Ok(ReportOutcome {
                link_id: existing.id,
                is_new: false,
                added_reporter: added,
                needs_processing: needs,
            });
        }

        // Link nou.
        let link = self.db.create_link(&url, user.id).await?;
        self.db.add_report(link.id, user.id).await?;

        Ok(ReportOutcome {
            link_id: link.id,
            is_new: true,
            added_reporter: true,
            needs_processing: true,
        })
    }

    /// Llança el pipeline en background.
    pub fn spawn_pipeline(&self, link_id: Uuid) {
        let db = self.db.clone();
        let cfg = self.cfg.clone();
        let http = self.http.clone();
        let llm = self.llm.clone();
        tokio::spawn(async move {
            let llm_ref = llm.as_deref();
            if let Err(e) = pipeline::process_link(&db, &cfg, &http, llm_ref, link_id).await {
                tracing::error!(%link_id, error = %e, "pipeline task failed");
            }
        });
    }

    /// Versió síncrona (espera) — útil per la CLI.
    pub async fn process_now(&self, link_id: Uuid) -> Result<()> {
        let llm_ref = self.llm.as_deref();
        pipeline::process_link(&self.db, &self.cfg, &self.http, llm_ref, link_id).await
    }
}
