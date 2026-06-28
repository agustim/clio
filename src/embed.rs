//! Generació d'embeddings, independent del LLM de chat.
//! Dos backends: HTTP (endpoint OpenAI-compatible /embeddings) i, opcionalment,
//! local in-process via `fastembed` (feature `local-embed`).

use crate::config::EmbedConfig;
use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};

/// Backend d'embeddings actiu.
pub enum Embedder {
    Http(HttpEmbedder),
    #[cfg(feature = "local-embed")]
    Local(LocalEmbedder),
}

impl Embedder {
    /// Genera l'embedding (vector de floats) d'un text.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        match self {
            Embedder::Http(h) => h.embed(text).await,
            #[cfg(feature = "local-embed")]
            Embedder::Local(l) => l.embed(text).await,
        }
    }
}

/// Construeix el backend segons la config. `None` si no està habilitat.
pub fn build(cfg: &EmbedConfig, http: reqwest::Client) -> Result<Option<Embedder>> {
    if !cfg.enabled() {
        return Ok(None);
    }
    if cfg.is_local() {
        #[cfg(feature = "local-embed")]
        {
            tracing::info!(model = %cfg.model, "embeddings: backend local (fastembed)");
            return Ok(Some(Embedder::Local(LocalEmbedder::new(&cfg.model))));
        }
        #[cfg(not(feature = "local-embed"))]
        {
            return Err(AppError::Config(
                "EMBED_PROVIDER=local requereix compilar amb la feature 'local-embed'".into(),
            ));
        }
    }
    tracing::info!(model = %cfg.model, base = %cfg.base_url, "embeddings: backend HTTP");
    Ok(Some(Embedder::Http(HttpEmbedder::new(http, cfg.clone()))))
}

// ---- HTTP (OpenAI-compatible) ----

#[derive(Serialize)]
struct EmbedReq<'a> {
    model: &'a str,
    input: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Deserialize)]
struct EmbedResp {
    data: Vec<EmbedData>,
}
#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

pub struct HttpEmbedder {
    http: reqwest::Client,
    cfg: EmbedConfig,
}

impl HttpEmbedder {
    pub fn new(http: reqwest::Client, cfg: EmbedConfig) -> Self {
        Self { http, cfg }
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let input: String = text.chars().take(8000).collect();
        let req = EmbedReq {
            model: &self.cfg.model,
            input: &input,
            dimensions: if self.cfg.dim > 0 { Some(self.cfg.dim) } else { None },
        };
        let url = format!("{}/embeddings", self.cfg.base_url.trim_end_matches('/'));
        let mut rb = self.http.post(&url).json(&req);
        if let Some(key) = &self.cfg.api_key {
            rb = rb.bearer_auth(key);
        }
        let resp = rb.send().await?.error_for_status()?;
        let body: EmbedResp = resp.json().await?;
        body.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| AppError::Pipeline("embed: resposta buida".into()))
    }
}

// ---- Local in-process (fastembed) ----

#[cfg(feature = "local-embed")]
pub use local::LocalEmbedder;

#[cfg(feature = "local-embed")]
mod local {
    use super::{AppError, Result};
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use std::sync::{Arc, Mutex};

    /// Model carregat de forma mandrosa (al primer ús): evita penalitzar
    /// comandes que no necessiten embeddings i descarrega el model un sol cop.
    pub struct LocalEmbedder {
        model_id: String,
        cell: Arc<Mutex<Option<TextEmbedding>>>,
    }

    impl LocalEmbedder {
        pub fn new(model_id: &str) -> Self {
            Self {
                model_id: model_id.to_string(),
                cell: Arc::new(Mutex::new(None)),
            }
        }

        pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let cell = self.cell.clone();
            let id = self.model_id.clone();
            // Els models e5 esperen el prefix "passage:" per a documents.
            let input = format!("passage: {}", text.chars().take(8000).collect::<String>());
            tokio::task::spawn_blocking(move || {
                let mut guard = cell
                    .lock()
                    .map_err(|_| AppError::Pipeline("embed: mutex enverinat".into()))?;
                if guard.is_none() {
                    let model = TextEmbedding::try_new(
                        InitOptions::new(map_model(&id)).with_show_download_progress(true),
                    )
                    .map_err(|e| AppError::Config(format!("fastembed init: {e}")))?;
                    *guard = Some(model);
                }
                let model = guard.as_mut().expect("model carregat");
                let mut out = model
                    .embed(vec![input], None)
                    .map_err(|e| AppError::Pipeline(format!("fastembed: {e}")))?;
                out.pop()
                    .ok_or_else(|| AppError::Pipeline("fastembed: sortida buida".into()))
            })
            .await
            .map_err(|e| AppError::Pipeline(format!("embed task: {e}")))?
        }
    }

    /// Mapeja l'id de model (string de config) a l'enum de fastembed.
    fn map_model(id: &str) -> EmbeddingModel {
        match id.to_lowercase().as_str() {
            "multilingual-e5-base" | "intfloat/multilingual-e5-base" => {
                EmbeddingModel::MultilingualE5Base
            }
            "multilingual-e5-large" | "intfloat/multilingual-e5-large" => {
                EmbeddingModel::MultilingualE5Large
            }
            "bge-m3" | "baai/bge-m3" => EmbeddingModel::BGEM3,
            "bge-small-en-v1.5" | "bge-small" => EmbeddingModel::BGESmallENV15,
            "nomic-embed-text-v1.5" | "nomic-embed-text" => EmbeddingModel::NomicEmbedTextV15,
            "all-minilm-l6-v2" => EmbeddingModel::AllMiniLML6V2,
            // Per defecte: multilingüe petit (bo per al català).
            _ => EmbeddingModel::MultilingualE5Small,
        }
    }
}
