use crate::config::LlmConfig;
use crate::error::{AppError, Result};
use crate::models::{Analysis, Sentiment};
use serde::{Deserialize, Serialize};

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
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let mut rb = self.http.post(&url).json(&req);
        if let Some(key) = &self.cfg.api_key {
            rb = rb.bearer_auth(key);
        }
        let resp = rb.send().await?.error_for_status()?;
        let body: ChatResp = resp.json().await?;
        body.choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| AppError::Pipeline("llm: empty choices".into()))
    }

    pub async fn analyze(&self, title: &str, text: &str, max_words: usize) -> Result<Analysis> {
        let prompt = format!(
            "Ets un analista de continguts. Resumeix el text en CATALÀ en menys de {max_words} paraules, \
             extreu entre 5 i 10 tags (minuscules, sense accents) i determina el sentiment global.\n\
             Respon NOMÉS amb JSON valid d'aquesta forma exacta:\n\
             {{\"summary\": \"...\", \"tags\": [\"a\",\"b\"], \"sentiment\": \"positive|neutral|negative\"}}\n\n\
             TÍTOL: {title}\n\nTEXT:\n{text}"
        );
        let req = ChatReq {
            model: &self.cfg.model,
            messages: vec![Msg { role: "user", content: &prompt }],
            temperature: 0.2,
        };
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let mut rb = self.http.post(&url).json(&req);
        if let Some(key) = &self.cfg.api_key {
            rb = rb.bearer_auth(key);
        }
        let resp = rb.send().await?.error_for_status()?;
        let body: ChatResp = resp.json().await?;
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
        Ok(Analysis {
            summary: parsed.summary,
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
