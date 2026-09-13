use crate::config::LlmConfig;
use crate::error::{AppError, Result};
use crate::models::{Analysis, Sentiment};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Reintents de la crida HTTP al model davant errors transitoris (timeout,
/// connexions tallades, 5xx...). Amb una fallada persistent no ens quedem amb
/// un fallback heurístic en l'idioma de la pàgina: l'error es propaga i el
/// link queda en 'failed' perquè es pugui reintentar amb «Refer».
const LLM_RETRIES: usize = 2;
/// Backoff exponencial base per als reintents (500ms · 2^n). Juntament amb el
/// circuit breaker i el rate limiter evita martellejar un endpoint caigut.
const LLM_RETRY_BASE_MS: u64 = 500;

/// Client OpenAI-compatible (vLLM / OpenAI / Ollama-openai).
///
/// Resilient davant un proveïdor degradat:
/// - **Circuit breaker** compartit entre tots els workers (`Arc<LlmClient>`):
///   si hi ha `circuit_threshold` fallades consecutives, s'obre el circuit i es
///   fan *fail-fast* (sense enviar HTTP) durant un cooldown, amb una única sonda
///   (half-open) en acabar-lo. Així un endpoint caigut no es martelleja i té
///   temps de recuperar-se sense que el drenatge del backlog el saturi.
/// - **Rate limiter** (token bucket comú): capa el nombre de crides per segon
///   perquè un backlog acumulat mai alluvi el model de cop.
pub struct LlmClient {
    http: reqwest::Client,
    cfg: LlmConfig,
    circuit: Mutex<Circuit>,
    rate: Mutex<RateBucket>,
}

/// Estad del circuit breaker.
struct Circuit {
    /// Fallades consecutives (es buiden amb la primera resposta vàlida).
    consecutive_fails: usize,
    /// Estat actual.
    state: CircuitState,
    /// Faillades consecutives que obren el circuit (0 desactiva el cooldown).
    threshold: usize,
    /// Durada del cooldown un cop obert.
    cooldown: Duration,
}

enum CircuitState {
    /// Normal: s'envien peticions.
    Closed,
    /// Refusa totes les crides fins `open_until`.
    Open { open_until: Instant },
    /// S'ha concedit UNA sonda; les altres crides es rebutgen fins que passi.
    HalfOpen,
}

/// Token bucket per capar les crides per segon (0.0 = sense límit).
struct RateBucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
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
    /// Resposta final. En models de raonament (DeepSeek-R1 & co.) sovint és
    /// `null` i el text real viu a `reasoning_content`; cal suportar-ho.
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

impl RespMsg {
    /// El text de la resposta: preferim `content`; si és buit/null (models de
    /// raonament), fem fallback a `reasoning_content`.
    fn text(&self) -> Option<&str> {
        self.content
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.reasoning_content
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
    }
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
        let circuit = Circuit {
            consecutive_fails: 0,
            state: CircuitState::Closed,
            threshold: cfg.circuit_threshold,
            cooldown: Duration::from_secs(cfg.circuit_cooldown_secs),
        };
        let rate = RateBucket {
            rate: cfg.rate_per_sec.max(0.0),
            burst: cfg.rate_per_sec.max(0.0).max(1.0),
            tokens: cfg.rate_per_sec.max(0.0).max(1.0),
            last: Instant::now(),
        };
        Self {
            http,
            cfg,
            circuit: Mutex::new(circuit),
            rate: Mutex::new(rate),
        }
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
            .and_then(|c| c.message.text().map(str::to_owned))
            .ok_or_else(|| AppError::Llm("llm: empty choices".into()))
    }

    /// Comprovació ràpida a l'arrencada: confirma que el model respon i amb
    /// quina forma (`content` vs `reasoning_content`) per saber com treballar-hi.
    /// És diagnòstic: NO toca el circuit breaker ni el rate limiter.
    pub async fn health_check(&self) -> ModelHealth {
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let req = ChatReq {
            model: &self.cfg.model,
            messages: vec![Msg { role: "user", content: "Respon només amb la paraula: ok" }],
            temperature: 0.0,
        };
        let mut rb = self
            .http
            .post(&url)
            .timeout(Duration::from_secs(15))
            .json(&req);
        if let Some(key) = &self.cfg.api_key {
            rb = rb.bearer_auth(key);
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                return ModelHealth {
                    reachable: false,
                    mode: ModelMode::Unknown,
                    error: Some(e.to_string()),
                }
            }
        };
        let body: ChatResp = match resp.json::<ChatResp>().await {
            Ok(b) => b,
            Err(e) => {
                return ModelHealth {
                    reachable: true,
                    mode: ModelMode::Unknown,
                    error: Some(format!("cos no desxifrable: {e}")),
                }
            }
        };
        let msg = match body.choices.into_iter().next() {
            Some(c) => c.message,
            None => {
                return ModelHealth {
                    reachable: true,
                    mode: ModelMode::Unknown,
                    error: Some("sense choices".into()),
                }
            }
        };
        let has_content = msg
            .content
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some();
        let has_reasoning = msg
            .reasoning_content
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some();
        let mode = if has_content {
            ModelMode::Content
        } else if has_reasoning {
            ModelMode::Reasoning
        } else {
            ModelMode::Unknown
        };
        ModelHealth {
            reachable: true,
            mode,
            error: if mode == ModelMode::Unknown {
                Some("resposta buida".into())
            } else {
                None
            },
        }
    }

    /// Crida HTTP al model amb reintent davant errors transitoris, limitada pel
    /// circuit breaker (fail-fast durant el cooldown) i pel rate limiter.
    ///
    /// Qualsevol fallada (timeout, connexió tallada, 5xx, cos no desxifrable...)
    /// es propaga com a `AppError::Llm`: el pipeline la distingeix de les
    /// fallades de *link* i no compta la URL com a dolenta ni enganxa l'admin.
    /// Les fallades transitoris es reintenten amb backoff exponencial; si al
    /// final tampoc respon, s'obre/continua obert el circuit.
    async fn chat(&self, req: &ChatReq<'_>) -> Result<ChatResp> {
        // 1) Rate limit compartit (no alluvi el model amb un backlog acumular).
        self.acquire_rate().await;

        // 2) Circuit breaker: fail-fast mentre està dins del cooldown.
        match self.circuit_gate()? {
            Gate::Proceed => {}
            Gate::Cooldown => {
                return Err(AppError::Llm("llm: servei en cooldown, es retarda el reintent".into()));
            }
        }

        // 3) Crides HTTP amb reintents i backoff exponencial.
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let mut last: Option<reqwest::Error> = None;
        let mut success = false;
        let mut body = None;
        for attempt in 0..=LLM_RETRIES {
            let mut rb = self
                .http
                .post(&url)
                .timeout(Duration::from_secs(self.cfg.timeout_secs))
                .json(req);
            if let Some(key) = &self.cfg.api_key {
                rb = rb.bearer_auth(key);
            }
            let attempt_res = async {
                let resp = rb.send().await?;
                let resp = resp.error_for_status()?;
                resp.json::<ChatResp>().await
            }
            .await;
            match attempt_res {
                Ok(b) => {
                    success = true;
                    body = Some(b);
                    break;
                }
                Err(e) => {
                    tracing::debug!(attempt, error = %e, "llm: crida fallida, es reintenta");
                    last = Some(e);
                    let delay = LLM_RETRY_BASE_MS * (1u64 << attempt);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }

        if success {
            self.circuit_success();
            Ok(body.expect("succés amb body"))
        } else {
            let err = match last {
                Some(e) => AppError::Llm(format!("crida HTTP fallida: {e}")),
                None => AppError::Llm("sense resposta".into()),
            };
            self.circuit_failure();
            Err(err)
        }
    }

    /// Fa esperar fins que hi hagi un token disponible (si `rate > 0`).
    async fn acquire_rate(&self) {
        let deficit = {
            let mut b = self.rate.lock().unwrap();
            if b.rate <= 0.0 {
                None
            } else {
                let now = Instant::now();
                let dt = (now - b.last).as_secs_f64();
                b.last = now;
                b.tokens = (b.tokens + dt * b.rate).min(b.burst);
                b.tokens -= 1.0;
                if b.tokens < 0.0 {
                    Some(Duration::from_secs_f64((-b.tokens) / b.rate))
                } else {
                    None
                }
            }
        };
        if let Some(d) = deficit {
            tokio::time::sleep(d).await;
        }
    }

    /// Retorna com tractem la crida segons l'estat del circuit.
    fn circuit_gate(&self) -> Result<Gate> {
        let mut c = self.circuit.lock().unwrap();
        if c.threshold == 0 || c.cooldown.is_zero() {
            return Ok(Gate::Proceed);
        }
        let now = Instant::now();
        match c.state {
            CircuitState::Closed => Ok(Gate::Proceed),
            CircuitState::Open { open_until } => {
                if now >= open_until {
                    // Cooldown vençut: concedim UNA sonda (half-open).
                    tracing::info!("llm: circuit mig-obert, sonda de prova");
                    c.state = CircuitState::HalfOpen;
                    Ok(Gate::Proceed)
                } else {
                    Ok(Gate::Cooldown)
                }
            }
            CircuitState::HalfOpen => Ok(Gate::Cooldown),
        }
    }

    /// Se crida quan una resposta del model és vàlida: tanca el circuit.
    fn circuit_success(&self) {
        let mut c = self.circuit.lock().unwrap();
        if c.consecutive_fails > 0 {
            tracing::info!(fails = c.consecutive_fails, "llm: circuit tancat (resposta vàlida)");
        }
        c.consecutive_fails = 0;
        c.state = CircuitState::Closed;
    }

    /// Cert si ara mateix el circuit rebutjaria crides (cooldown actiu o sonda
    /// en vol). Ho fa servir el "reaper" per no re-encuar links inútilment
    /// mentre el model està caigut.
    pub fn is_cooling_down(&self) -> bool {
        let c = self.circuit.lock().unwrap();
        match c.state {
            CircuitState::Open { open_until } => Instant::now() < open_until,
            CircuitState::HalfOpen => true,
            CircuitState::Closed => false,
        }
    }

    /// Se crida quan el model falla: acumula fallades i obre el circuit en
    /// arribar al llindar (amb cooldown i backoff).
    fn circuit_failure(&self) {
        let mut c = self.circuit.lock().unwrap();
        c.consecutive_fails += 1;
        let now = Instant::now();
        let enabled = c.threshold > 0 && !c.cooldown.is_zero();
        let was_half_open = matches!(c.state, CircuitState::HalfOpen);
        let threshold_reached = c.consecutive_fails >= c.threshold;
        if enabled && (was_half_open || threshold_reached) {
            // Obre/reobre el circuit. Capem el comptador perquè no creixi sense
            // límit durant una caiguda llarga (el missatge no ha de dir "milers"
            // de fallades "seguides": el circuit porta estona obert).
            c.consecutive_fails = c.threshold.max(1);
            let msg = if was_half_open {
                "llm: sonda fallida, circuit reobert"
            } else {
                "llm: circuit OBERT (massa fallades seguides), pausa abans de tornar-ho a provar"
            };
            tracing::warn!(threshold = c.threshold, cooldown_secs = c.cooldown.as_secs(), "{msg}");
            c.state = CircuitState::Open {
                open_until: now + c.cooldown,
            };
        }
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
            .and_then(|c| c.message.text().map(str::to_owned))
            .ok_or_else(|| AppError::Llm("llm: empty choices".into()))?;

        let json_str = extract_json(&content)
            .ok_or_else(|| AppError::Llm("llm: no JSON in response".into()))?;
        let parsed: LlmAnalysis = serde_json::from_str(json_str)
            .map_err(|e| AppError::Llm(format!("llm: bad JSON: {e}")))?;

        let sentiment = match parsed.sentiment.to_lowercase().as_str() {
            "positive" => Sentiment::Positive,
            "negative" => Sentiment::Negative,
            _ => Sentiment::Neutral,
        };
        let title = {
            let t = parsed.title.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        let summary = parsed.summary.trim().to_string();
        // Salvaguarda de llengua: si la resposta és buida (ni títol ni resum),
        // la tractem com a fallada del LLM. No publiquem cap fallback heurístic
        // que copiï l'idioma original de la pàgina: l'error es propaga i el
        // link queda en 'failed', llest per reenquar-se amb «Refer».
        if title.is_none() && summary.is_empty() {
            return Err(AppError::Llm("llm: resposta buida (sense títol ni resum)".into()));
        }
        Ok(Analysis {
            title,
            summary,
            tags: parsed.tags,
            sentiment,
        })
    }
}

#[derive(PartialEq)]
enum Gate {
    Proceed,
    Cooldown,
}

/// Com respon el proveïdor LLM: com saber "llegir" la seva sortida.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelMode {
    /// Resposta normal dins `content`.
    Content,
    /// Model de raonament: resposta a `reasoning_content` (i `content` buit/null).
    Reasoning,
    /// No s'ha pogut determinar (resposta buida o desconeguda).
    Unknown,
}

/// Resultat de la comprovació de salut del model a l'arrencada.
#[derive(Debug)]
pub struct ModelHealth {
    pub reachable: bool,
    pub mode: ModelMode,
    pub error: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    // Models de raonament (DeepSeek-R1 & co.) retornen la resposta a
    // `reasoning_content` i `content: null`. Clio ha de desxifrar-ho, no caure
    // com a "error decoding response body" (que era el símptoma real a producció).
    #[test]
    fn resp_msg_prefers_content_falls_back_to_reasoning() {
        // content present => el fem servir.
        let m: RespMsg =
            serde_json::from_str(r#"{"role":"assistant","content":"Resposta normal"}"#).unwrap();
        assert_eq!(m.text(), Some("Resposta normal"));

        // content: null => raonament.
        let m: RespMsg = serde_json::from_str(
            r#"{"role":"assistant","reasoning_content":"Resposta del model","content":null}"#,
        )
        .unwrap();
        assert_eq!(m.text(), Some("Resposta del model"));

        // només reasoning_content (sense content) => raonament.
        let m: RespMsg =
            serde_json::from_str(r#"{"role":"assistant","reasoning_content":"Raonat"}"#).unwrap();
        assert_eq!(m.text(), Some("Raonat"));

        // content buit i reasoning buit => None.
        let m: RespMsg = serde_json::from_str(r#"{"role":"assistant"}"#).unwrap();
        assert_eq!(m.text(), None);
    }
}
