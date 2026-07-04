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
    /// Avisos d'admin via Telegram (None si no configurat).
    pub notifier: Option<Arc<crate::telegram::Notifier>>,
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
        let notifier =
            crate::telegram::Notifier::build(&cfg.telegram_bot_token, cfg.admin_chat_id).map(Arc::new);
        let (queue, rx) = queue::start();
        let state = Self {
            db,
            cfg: Arc::new(cfg),
            http,
            llm,
            embedder,
            queue,
            notifier,
            web_dirty: Arc::new(tokio::sync::Notify::new()),
        };
        Ok((state, rx))
    }

    /// Logica de recepció + deduplicació + co-reporting.
    /// NO processa: retorna `needs_processing` perquè el caller decideixi
    /// (API => encua a la cua, CLI => síncron inline).
    pub async fn report_link(&self, user: &User, raw_url: &str) -> Result<ReportOutcome> {
        let url = normalize_url(raw_url)?;

        // Blocklist: rebutja URLs que facin match amb algun patró (regex).
        if let Some(pat) = self.blocklist_match(&url).await {
            return Err(crate::error::AppError::Blocked(pat));
        }

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

    /// Retorna el primer patró de la blocklist que fa match amb la URL, si n'hi
    /// ha cap. Els patrons són regex; els invàlids es registren i s'ignoren.
    async fn blocklist_match(&self, url: &str) -> Option<String> {
        let patterns = match self.db.blocklist_patterns().await {
            Ok(p) if !p.is_empty() => p,
            Ok(_) => return None,
            Err(e) => {
                tracing::warn!(error = %e, "blocklist: lectura fallida");
                return None;
            }
        };
        for pat in patterns {
            match regex::Regex::new(&pat) {
                Ok(re) if re.is_match(url) => return Some(pat),
                Ok(_) => {}
                Err(e) => tracing::warn!(pattern = %pat, error = %e, "blocklist: regex invàlid"),
            }
        }
        None
    }

    /// Encua la primera passada (shallow) a la cua de workers.
    pub fn enqueue(&self, link_id: Uuid) {
        self.queue.shallow(link_id);
    }

    /// Envia un avís d'admin si hi ha notifier configurat (best-effort).
    pub async fn notify(&self, text: &str) {
        if let Some(n) = &self.notifier {
            n.send(text).await;
        }
    }

    /// Avís d'error amb botons per esborrar o reintentar el link.
    pub async fn notify_error(&self, text: &str, link_id: Uuid) {
        if let Some(n) = &self.notifier {
            n.send_error(text, link_id).await;
        }
    }

    /// Bloqueja el link: afegeix la seva URL (exacta) a la blocklist i
    /// l'esborra. Retorna false si el link ja no existeix.
    pub async fn block_link(&self, link_id: Uuid) -> Result<bool> {
        let Some(link) = self.db.link_by_id(link_id).await? else {
            return Ok(false);
        };
        let pattern = format!("^{}$", regex::escape(&link.url));
        // Un patró duplicat (ja bloquejat) no ha d'impedir l'esborrat.
        if let Err(e) = self.db.add_block(&pattern, Some("via telegram")).await {
            tracing::warn!(error = %e, %pattern, "block_link: add_block (potser duplicat)");
        }
        self.db.delete_link(link_id).await?;
        Ok(true)
    }

    /// Reencua un link: torna l'estat a Pending i el posa a la cua shallow.
    pub async fn retry_link(&self, link_id: Uuid) -> Result<()> {
        self.db.set_link_status(link_id, LinkStatus::Pending).await?;
        self.enqueue(link_id);
        Ok(())
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
