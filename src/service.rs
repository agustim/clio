use crate::config::Config;
use crate::db::Db;
use crate::deep;
use crate::embed::Embedder;
use crate::error::Result;
use crate::llm::LlmClient;
use crate::models::{DeepStatus, LinkStatus, User};
use crate::normalize::normalize_url;
use crate::pipeline;
use crate::queue::{self, Job, Queue};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Estat compartit per API, CLI i Bot.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
    pub llm: Option<Arc<LlmClient>>,
    pub embedder: Option<Arc<Embedder>>,
    pub queue: Queue,
    /// Senyal: el contingut web ha canviat i cal regenerar/desplegar.
    pub web_dirty: Arc<tokio::sync::Notify>,
}

pub struct ReportOutcome {
    pub link_id: Uuid,
    pub is_new: bool,
    pub added_reporter: bool,
    /// El link cal (re)processar-lo. El caller decideix com (cua vs síncron).
    pub needs_processing: bool,
}

impl AppState {
    /// Construeix l'estat i retorna també el Receiver de la cua, que el
    /// dispatcher (`queue::run`) ha de consumir (només quan hi ha workers, ex. `serve`).
    pub fn new(db: Db, cfg: Config) -> Result<(Self, mpsc::Receiver<Job>)> {
        let http = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let llm = pipeline::build_llm(&cfg, http.clone());
        let embedder = crate::embed::build(&cfg.embed, http.clone())?.map(Arc::new);
        let (queue, rx) = queue::start();
        let state = Self {
            db,
            cfg: Arc::new(cfg),
            http,
            llm,
            embedder,
            queue,
            web_dirty: Arc::new(tokio::sync::Notify::new()),
        };
        Ok((state, rx))
    }

    /// Logica de recepció + deduplicació + co-reporting.
    /// NO processa: retorna `needs_processing` perquè el caller decideixi
    /// (API => encua a la cua, CLI => síncron inline).
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

    /// Encua la primera passada (shallow) a la cua de workers.
    pub fn enqueue(&self, link_id: Uuid) {
        self.queue.shallow(link_id);
    }

    /// Re-encua tota la feina pendent de la DB (recovery en arrencar).
    pub async fn recover(&self) -> Result<()> {
        let shallow = self.db.pending_shallow_ids().await?;
        let deep = self.db.pending_deep_ids().await?;
        for id in &shallow {
            self.queue.shallow(*id);
        }
        for id in &deep {
            self.queue.deep(*id);
        }
        if !shallow.is_empty() || !deep.is_empty() {
            tracing::info!(shallow = shallow.len(), deep = deep.len(), "recovered pending jobs");
        }
        Ok(())
    }

    /// Backfill d'embeddings: genera'ls per a tots els links `done` que en
    /// manquin. Retorna (generats, total_pendents). No-op sense LLM.
    pub async fn reindex_embeddings(&self) -> Result<(usize, usize)> {
        let Some(emb) = self.embedder.as_deref() else {
            return Ok((0, 0));
        };
        let ids = self.db.missing_embedding_ids().await?;
        let total = ids.len();
        let mut done = 0usize;
        for id in ids {
            if let Some(l) = self.db.link_by_id(id).await? {
                let text = format!(
                    "{}\n{}\n{}",
                    l.title.unwrap_or_default(),
                    l.summary.unwrap_or_default(),
                    l.tags.join(" ")
                );
                match pipeline::embed_and_store(&self.db, emb, id, &text).await {
                    Ok(_) => done += 1,
                    Err(e) => tracing::warn!(%id, error = %e, "reindex embed failed"),
                }
            }
        }
        Ok((done, total))
    }

    /// Processament complet inline (shallow + deep) — usat per la CLI, que no
    /// té workers en marxa. Espera fins acabar.
    pub async fn process_full(&self, link_id: Uuid) -> Result<()> {
        let llm = self.llm.as_deref();
        let embedder = self.embedder.as_deref();
        pipeline::process_link(&self.db, &self.cfg, &self.http, llm, embedder, link_id).await?;

        if let Some(link) = self.db.link_by_id(link_id).await? {
            if link.deep_applicable() {
                self.db.set_deep_status(link_id, DeepStatus::Pending).await?;
                deep::process_deep(&self.db, &self.cfg, &self.http, llm, link_id).await?;
            }
        }
        Ok(())
    }
}
