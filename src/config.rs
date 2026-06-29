use crate::error::{AppError, Result};

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub provider: String, // vllm | openai | ollama | none
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    /// Timeout per crida al LLM (segons). Generació pot trigar molt en models grans.
    pub timeout_secs: u64,
}

impl LlmConfig {
    /// Considerat actiu si hi ha provider != none/buit i base_url.
    pub fn enabled(&self) -> bool {
        !self.provider.is_empty()
            && self.provider != "none"
            && self.provider != "local"
            && !self.base_url.is_empty()
    }
}

/// Configuració d'embeddings, independent del LLM de chat.
#[derive(Debug, Clone)]
pub struct EmbedConfig {
    /// none | local (fastembed in-process) | openai | vllm | ollama | http
    pub provider: String,
    /// Per a HTTP: nom del model remot. Per a local: id del model fastembed.
    pub model: String,
    /// Endpoint OpenAI-compatible (/embeddings). Ignorat en mode local.
    pub base_url: String,
    pub api_key: Option<String>,
    /// Dimensions demanades (només OpenAI v3 honora `dimensions`). 0 = no enviar.
    pub dim: usize,
}

impl EmbedConfig {
    pub fn is_local(&self) -> bool {
        self.provider == "local"
    }
    /// Actiu si hi ha algun proveïdor configurat (local, o HTTP amb base_url).
    pub fn enabled(&self) -> bool {
        if self.provider.is_empty() || self.provider == "none" {
            return false;
        }
        self.is_local() || !self.base_url.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct GitConfig {
    pub web_repo_url: Option<String>,
    pub web_branch: String,
    pub git_token: Option<String>,
}

impl GitConfig {
    pub fn push_enabled(&self) -> bool {
        self.web_repo_url.as_ref().map(|s| !s.is_empty()).unwrap_or(false)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub llm: LlmConfig,
    pub embed: EmbedConfig,
    pub git: GitConfig,
    pub telegram_bot_token: Option<String>,
    /// Chat de Telegram on el bot envia avisos d'admin (errors, arrencada).
    /// Buit = sense notificacions. Pot ser negatiu (grups).
    pub admin_chat_id: Option<i64>,
    pub public_dir: String,
    pub max_link_size_bytes: usize,
    pub summary_max_words: usize,
    pub user_agent: String,
    /// Nombre de workers concurrents de la cua d'anàlisi.
    pub queue_workers: usize,
    /// Interval (segons) de regeneració de la web durant `serve`. 0 = desactivat.
    pub web_regen_secs: u64,
    /// Finestra (segons) per agrupar una ràfega de links nous en un sol
    /// deploy reactiu. S'aplica després del senyal `web_dirty`.
    pub web_debounce_secs: u64,
    /// Límits per a la segona passada (clone de repos).
    pub clone_timeout_secs: u64,
    pub clone_max_mb: u64,
}

fn opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn get(key: &str, default: &str) -> String {
    opt(key).unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let max_mb: usize = get("MAX_LINK_SIZE_MB", "5")
            .parse()
            .map_err(|_| AppError::Config("MAX_LINK_SIZE_MB invalid".into()))?;
        let summary_max_words: usize = get("SUMMARY_MAX_WORDS", "300")
            .parse()
            .map_err(|_| AppError::Config("SUMMARY_MAX_WORDS invalid".into()))?;
        let queue_workers: usize = get("QUEUE_WORKERS", "4")
            .parse()
            .map_err(|_| AppError::Config("QUEUE_WORKERS invalid".into()))?;
        let web_regen_secs: u64 = get("WEB_REGEN_SECS", "30")
            .parse()
            .map_err(|_| AppError::Config("WEB_REGEN_SECS invalid".into()))?;
        let web_debounce_secs: u64 = get("WEB_DEBOUNCE_SECS", "60")
            .parse()
            .map_err(|_| AppError::Config("WEB_DEBOUNCE_SECS invalid".into()))?;
        let clone_timeout_secs: u64 = get("CLONE_TIMEOUT_SECS", "120")
            .parse()
            .map_err(|_| AppError::Config("CLONE_TIMEOUT_SECS invalid".into()))?;
        let clone_max_mb: u64 = get("CLONE_MAX_MB", "200")
            .parse()
            .map_err(|_| AppError::Config("CLONE_MAX_MB invalid".into()))?;
        let embed_dim: usize = get("EMBED_DIM", "256")
            .parse()
            .map_err(|_| AppError::Config("EMBED_DIM invalid".into()))?;

        // LLM de chat.
        let llm_provider = get("LLM_PROVIDER", "none");
        let llm_base = get("LLM_BASE_URL", "http://localhost:8000/v1");
        let llm_key = opt("LLM_API_KEY");
        let llm_timeout_secs: u64 = get("LLM_TIMEOUT_SECS", "120")
            .parse()
            .map_err(|_| AppError::Config("LLM_TIMEOUT_SECS invalid".into()))?;

        // Embeddings: provider propi. Si no s'especifica base_url/api_key,
        // es reusen els del LLM de chat (ergonòmic per a setups d'un sol proveïdor).
        let embed = EmbedConfig {
            provider: get("EMBED_PROVIDER", &llm_provider),
            model: get("EMBED_MODEL", "multilingual-e5-small"),
            base_url: opt("EMBED_BASE_URL").unwrap_or_else(|| llm_base.clone()),
            api_key: opt("EMBED_API_KEY").or_else(|| llm_key.clone()),
            dim: embed_dim,
        };

        Ok(Config {
            database_url: get("DATABASE_URL", "sqlite://data/linkanalyzer.db"),
            bind_addr: get("BIND_ADDR", "127.0.0.1:8080"),
            llm: LlmConfig {
                provider: llm_provider,
                model: get("LLM_MODEL", "gpt-3.5-turbo"),
                base_url: llm_base,
                api_key: llm_key,
                timeout_secs: llm_timeout_secs,
            },
            embed,
            git: GitConfig {
                web_repo_url: opt("WEB_REPO_URL"),
                web_branch: get("WEB_BRANCH", "main"),
                git_token: opt("GIT_TOKEN"),
            },
            telegram_bot_token: opt("TELEGRAM_BOT_TOKEN"),
            admin_chat_id: opt("ADMIN_CHAT_ID").and_then(|s| s.parse().ok()),
            public_dir: get("PUBLIC_DIR", "public"),
            max_link_size_bytes: max_mb * 1024 * 1024,
            summary_max_words,
            user_agent: get("USER_AGENT", "Clio-LinkAnalyzer/0.1"),
            queue_workers,
            web_regen_secs,
            web_debounce_secs,
            clone_timeout_secs,
            clone_max_mb,
        })
    }
}
