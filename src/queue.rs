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
                Err(e) => tracing::error!(link_id = %job.link_id, error = %e, "shallow job failed"),
            }
        }
        Stage::Deep => {
            match deep::process_deep(&state.db, &state.cfg, &state.http, llm, job.link_id).await {
                Ok(()) => state.web_dirty.notify_one(),
                Err(e) => tracing::error!(link_id = %job.link_id, error = %e, "deep job failed"),
            }
        }
    }
}
