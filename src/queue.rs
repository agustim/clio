use crate::deep;
use crate::pipeline;
use crate::service::AppState;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Primera passada: fetch + parse + classify + analyze.
    Shallow,
    /// Segona passada: clone repo + codi, o text complet d'article.
    Deep,
}

#[derive(Debug, Clone, Copy)]
pub struct Job {
    pub link_id: Uuid,
    pub stage: Stage,
}

/// Handle clonable per encuar feina.
#[derive(Clone)]
pub struct Queue {
    tx: mpsc::Sender<Job>,
}

impl Queue {
    pub fn enqueue(&self, job: Job) {
        // try_send no bloqueja; si la cua és plena, ho fem en una task.
        if let Err(mpsc::error::TrySendError::Full(job)) = self.tx.try_send(job) {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(job).await;
            });
        }
    }

    pub fn shallow(&self, link_id: Uuid) {
        self.enqueue(Job { link_id, stage: Stage::Shallow });
    }
    pub fn deep(&self, link_id: Uuid) {
        self.enqueue(Job { link_id, stage: Stage::Deep });
    }
}

/// Crea la cua. Retorna el handle (va dins de l'AppState) i el Receiver
/// que consumirà el dispatcher (`run`).
pub fn start() -> (Queue, mpsc::Receiver<Job>) {
    let (tx, rx) = mpsc::channel::<Job>(1024);
    (Queue { tx }, rx)
}

/// Bucle dispatcher: limita la concurrència amb un semàfor i fa spawn per job.
pub async fn run(state: AppState, mut rx: mpsc::Receiver<Job>, workers: usize) {
    let sem = Arc::new(Semaphore::new(workers.max(1)));
    tracing::info!(workers, "queue workers ready");

    while let Some(job) = rx.recv().await {
        let permit = sem.clone().acquire_owned().await.expect("semaphore closed");
        let st = state.clone();
        tokio::spawn(async move {
            handle(&st, job).await;
            drop(permit);
        });
    }
}

/// Fallades consecutives d'una mateixa URL (font `delete_on_fail`) abans que
/// entri automàticament a la blocklist i deixi de col·lectar-se.
const NUM_ERRORS_TO_BLACKLIST: i64 = 5;

/// Gestiona una feina fallida. Els links reportats per un NPC no molesten
/// mai l'admin: es compten fallades consecutives (amb auto-esborrat si la
/// font és `delete_on_fail`) i, en arribar al llindar, la URL entra a la
/// blocklist. Els links humans avisen amb botons perquè l'admin decideixi
/// (esborrar / reintentar).
async fn notify_failure(state: &AppState, link_id: Uuid, what: &str, err: &str) {
    let url = match state.db.link_by_id(link_id).await {
        Ok(Some(l)) => l.url,
        _ => link_id.to_string(),
    };

    match state.db.link_from_npc(link_id).await {
        Ok(true) => {
            // Auto-esborrat: fonts marcades com a delete_on_fail no s'acumulen en 'failed'.
            match state.db.link_from_delete_on_fail_source(link_id).await {
                Ok(true) => match state.db.delete_link(link_id).await {
                    Ok(_) => tracing::info!(%link_id, %url, "auto-esborrat (font delete_on_fail)"),
                    Err(e) => tracing::warn!(error = %e, %link_id, "auto-esborrat fallit"),
                },
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, %link_id, "consulta delete_on_fail fallida"),
            }
            // Fallades consecutives (el comptador sobreviu l'esborrat). En
            // arribar al llindar, blocklist automàtica perquè deixi d'acceptar-se.
            match state.db.bump_url_fail_count(&url).await {
                Ok(n) if n >= NUM_ERRORS_TO_BLACKLIST => auto_blocklist(state, &url, n).await,
                Ok(n) => tracing::info!(%url, n, err, "fallada NPC ({n}/{NUM_ERRORS_TO_BLACKLIST}), sense avís"),
                Err(e) => tracing::warn!(error = %e, %url, "bump fail count fallit"),
            }
            return;
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, %link_id, "consulta reporter NPC fallida"),
    }

    state.notify_error(&format!("⚠️ {what} fallida\n{url}\n{err}"), link_id).await;
}

/// Afegeix la URL (exacta) a la blocklist després de massa fallades seguides.
/// Silenciós de cara a l'admin (només tracing): les fonts NPC no avisen mai.
async fn auto_blocklist(state: &AppState, url: &str, n: i64) {
    let pattern = format!("^{}$", regex::escape(url));
    let note = format!("auto: {n} fallades seguides");
    match state.db.add_block(&pattern, Some(&note)).await {
        Ok(_) => tracing::info!(%url, n, "auto-blocklist (massa fallades)"),
        // Un patró duplicat (ja bloquejat) no és un error real.
        Err(e) => tracing::warn!(error = %e, %pattern, "auto-blocklist: add_block (potser duplicat)"),
    }
    // Ja no cal el comptador un cop bloquejada.
    if let Err(e) = state.db.clear_url_fail_count(url).await {
        tracing::warn!(error = %e, %url, "clear fail count fallit");
    }
}

async fn handle(state: &AppState, job: Job) {
    let llm = state.llm.as_deref();
    let embedder = state.embedder.as_deref();
    match job.stage {
        Stage::Shallow => {
            match pipeline::process_link(&state.db, &state.cfg, &state.http, llm, embedder, job.link_id).await {
                Ok(()) => {
                    // Encua la segona passada si aplica.
                    if let Ok(Some(link)) = state.db.link_by_id(job.link_id).await {
                        if link.deep_applicable() {
                            let _ = state
                                .db
                                .set_deep_status(job.link_id, crate::models::DeepStatus::Pending)
                                .await;
                            state.queue.deep(job.link_id);
                        }
                    }
                    // Contingut nou publicable: dispara deploy reactiu (debounced).
                    state.web_dirty.notify_one();
                }
                Err(e) => {
                    tracing::error!(link_id = %job.link_id, error = %e, "shallow job failed");
                    notify_failure(state, job.link_id, "Anàlisi", &e.to_string()).await;
                }
            }
        }
        Stage::Deep => {
            match deep::process_deep(&state.db, &state.cfg, &state.http, llm, job.link_id).await {
                Ok(()) => state.web_dirty.notify_one(),
                Err(e) => {
                    tracing::error!(link_id = %job.link_id, error = %e, "deep job failed");
                    notify_failure(state, job.link_id, "Anàlisi profunda", &e.to_string()).await;
                }
            }
        }
    }
}
