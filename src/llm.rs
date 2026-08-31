use crate::config::LlmConfig;
use crate::error::{AppError, Result};
use crate::models::{Analysis, Sentiment};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Reintents de la crida HTTP al model davant errors transitoris (timeout,
/// connexions tallades, 5xx...). Amb una fallada persistent no ens quedem amb
/// un fallback heurístic en l'idioma de la pàgina: l'error es propaga i el
/// link queda en 'failed' perquè es pugui reintentar amb «Refer».
const LLM_RETRIES: usize = 2;
const LLM_RETRY_DELAY_MS: u64 = 500;

/// Client OpenAI-compatible (vLLM / OpenAI / Ollama-openai).
pub struct LlmClient {
    http: reqwest::Client,
    cfg: LlmConfig,
}

#[derive(Serialize)]
struct ChatReq<'a> {
    model: &'a str,
    messages: Vec<Msg<'a>>,
    temperature: f32,
}

#[derive(Serialize)]
struct Msg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: RespMsg,
}
#[derive(Deserialize)]
struct RespMsg {
    content: String,
}

/// Forma JSON que demanem al model.
#[derive(Deserialize)]
struct LlmAnalysis {
    #[serde(default)]
    title: String,
    summary: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    sentiment: String,
}

impl LlmClient {
    pub fn new(http: reqwest::Client, cfg: LlmConfig) -> Self {
        Self { http, cfg }
    }

    /// Completació lliure: retorna el text de la resposta del model.
    pub async fn complete(&self, prompt: &str) -> Result<String> {
        let req = ChatReq {
            model: &self.cfg.model,
            messages: vec![Msg { role: "user", content: prompt }],
            temperature: 0.3,
        };
        let body = self.chat(&req).await?;
        body.choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| AppError::Pipeline("llm: empty choices".into()))
    }

    /// Crida HTTP al model amb reintent davant errors transitoris. Si al final
    /// el model no respon, es retorna l'últim error: el pipeline el tracta com
    /// a fallada (link 'failed' reintentable) en lloc de publicar un fallback
    /// heurístic que copiaria el text original en la llengua de la pàgina.
    async fn chat(&self, req: &ChatReq<'_>) -> Result<ChatResp> {
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let mut last: Option<AppError> = None;
        for attempt in 0..=LLM_RETRIES {
            let mut rb = self
                .http
                .post(&url)
                .timeout(std::time::Duration::from_secs(self.cfg.timeout_secs))
                .json(&req);
            if let Some(key) = &self.cfg.api_key {
                rb = rb.bearer_auth(key);
            }
            // Bloc async per encadenar send -> error_for_status -> json sense
            // fer await dins d'un closure no async.
            let attempt_res = async {
                let resp = rb.send().await?;
                let resp = resp.error_for_status()?;
                resp.json::<ChatResp>().await
            }
            .await;
            match attempt_res {
                Ok(body) => return Ok(body),
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "llm: crida fallida, es reintenta");
                    last = Some(e.into());
                    tokio::time::sleep(Duration::from_millis(
                        LLM_RETRY_DELAY_MS * (attempt as u64 + 1),
                    ))
                    .await;
                }
            }
        }
        Err(last.unwrap_or_else(|| AppError::Pipeline("llm: sense resposta".into())))
    }

    pub async fn analyze(&self, title: &str, text: &str, max_chars: usize) -> Result<Analysis> {
        let prompt = format!(
            "Ets un analista de continguts. IMPORTANT: TOT el text que generis (títol, resum i \
             tags) ha d'estar integrament en CATALÀ.\n\
             - Genera un títol curt, periodístic i en català (màxim 80 caràcters, sense cometes).\n\
             - Resumeix el text en català en màxim {max_chars} caràcters amb una única frase de \
             PROSA PERIODÍSTICA que comenci directament pel contingut. No obris mai amb \
             presentacions metalingüístiques com «L’article descriu...», «Aquest text...», \
             «L’anàlisi de l’article...» ni similars.\n\
             - Sé fidel al text: no afegeixis fets, xifres, cites ni opinions que no hi surtin.\n\
             - Extreu entre 5 i 10 tags (minúscules, sense accents) i determina el sentiment global.\n\
             Respon NOMÉS amb JSON válid d'aquesta forma exacta:\n\
             {{\"title\": \"...\", \"summary\": \"...\", \"tags\": [\"a\",\"b\"], \"sentiment\": \"positive|neutral|negative\"}}\n\n\
             TÍTOL ORIGINAL: {title}\n\nTEXT:\n{text}"
        );
        let req = ChatReq {
            model: &self.cfg.model,
            messages: vec![Msg { role: "user", content: &prompt }],
            temperature: 0.2,
        };
        let body = self.chat(&req).await?;
        let content = body
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| AppError::Pipeline("llm: empty choices".into()))?;

        let json_str = extract_json(&content)
            .ok_or_else(|| AppError::Pipeline("llm: no JSON in response".into()))?;
        let parsed: LlmAnalysis = serde_json::from_str(json_str)
            .map_err(|e| AppError::Pipeline(format!("llm: bad JSON: {e}")))?;

        let sentiment = match parsed.sentiment.to_lowercase().as_str() {
            "positive" => Sentiment::Positive,
            "negative" => Sentiment::Negative,
            _ => Sentiment::Neutral,
        };
        let title = {
            let t = parsed.title.trim();
            if t.is_empty() { None } else { Some(t.to_string()) }
        };
        let summary = parsed.summary.trim().to_string();
        // Salvaguarda de llengua: si la resposta és buida (ni títol ni resum),
        // la tractem com a fallada del LLM. No publiquem cap fallback heurístic
        // que copiï l'idioma original de la pàgina: l'error es propaga i el
        // link queda en 'failed', llest per reenquar-se amb «Refer».
        if title.is_none() && summary.is_empty() {
            return Err(AppError::Pipeline("llm: resposta buida (sense títol ni resum)".into()));
        }
        Ok(Analysis {
            title,
            summary,
            tags: parsed.tags,
            sentiment,
        })
    }
}

/// Treu el primer bloc {...} d'una resposta (per si el model afegeix text al voltant).
fn extract_json(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end > start {
        Some(&s[start..=end])
    } else {
        None
    }
}
