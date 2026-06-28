use crate::error::{AppError, Result};

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub provider: String, // vllm | openai | ollama | none
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
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
    pub git: GitConfig,
    pub telegram_bot_token: Option<String>,
    pub public_dir: String,
    pub max_link_size_bytes: usize,
    pub summary_max_words: usize,
    pub user_agent: String,
    /// Nombre de workers concurrents de la cua d'anàlisi.
    pub queue_workers: usize,
    /// Interval (segons) de regeneració de la web durant `serve`. 0 = desactivat.
    pub web_regen_secs: u64,
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
        let clone_timeout_secs: u64 = get("CLONE_TIMEOUT_SECS", "120")
            .parse()
            .map_err(|_| AppError::Config("CLONE_TIMEOUT_SECS invalid".into()))?;
        let clone_max_mb: u64 = get("CLONE_MAX_MB", "200")
            .parse()
            .map_err(|_| AppError::Config("CLONE_MAX_MB invalid".into()))?;

        Ok(Config {
            database_url: get("DATABASE_URL", "sqlite://data/linkanalyzer.db"),
            bind_addr: get("BIND_ADDR", "127.0.0.1:8080"),
            llm: LlmConfig {
                provider: get("LLM_PROVIDER", "none"),
                model: get("LLM_MODEL", "gpt-3.5-turbo"),
                base_url: get("LLM_BASE_URL", "http://localhost:8000/v1"),
                api_key: opt("LLM_API_KEY"),
            },
            git: GitConfig {
                web_repo_url: opt("WEB_REPO_URL"),
                web_branch: get("WEB_BRANCH", "main"),
                git_token: opt("GIT_TOKEN"),
            },
            telegram_bot_token: opt("TELEGRAM_BOT_TOKEN"),
            public_dir: get("PUBLIC_DIR", "public"),
            max_link_size_bytes: max_mb * 1024 * 1024,
            summary_max_words,
            user_agent: get("USER_AGENT", "Clio-LinkAnalyzer/0.1"),
            queue_workers,
            web_regen_secs,
            clone_timeout_secs,
            clone_max_mb,
        })
    }
}
