use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Macro: enum <-> string per a serde (lowercase) i columnes TEXT de SQLite.
macro_rules! str_enum {
    ($name:ident { $($variant:ident => $s:literal),+ $(,)? }, default = $default:ident) => {
        #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $($variant),+
        }
        #[allow(dead_code)] // alguns variants/mètodes només els usa codi futur (Telegram, audit)
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
            pub fn from_db(s: &str) -> Self {
                match s { $($s => $name::$variant,)+ _ => $name::$default }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

str_enum!(UserRole { Admin => "admin", User => "user" }, default = User);
str_enum!(LinkType {
    News => "news", Repo => "repo", Article => "article",
    Video => "video", Blog => "blog", Social => "social", Other => "other"
}, default = Other);
str_enum!(Sentiment {
    Positive => "positive", Neutral => "neutral", Negative => "negative"
}, default = Neutral);
str_enum!(LinkStatus {
    Pending => "pending", Processing => "processing", Done => "done", Failed => "failed"
}, default = Pending);
str_enum!(DeepStatus {
    None => "none", Pending => "pending", Processing => "processing", Done => "done", Failed => "failed"
}, default = None);
// Audit-trail dels reports: el model existeix (spec) però encara no es
// llegeix de la DB, només s'hi insereix. Mantenim el tipus per a quan s'usi.
str_enum!(ReportStatus {
    Pending => "pending", Processed => "processed", Failed => "failed"
}, default = Pending);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    #[serde(skip_serializing)]
    pub api_token: String,
    pub role: UserRole,
    /// Id numèric de Telegram (com a text). El bot només accepta links
    /// d'usuaris amb aquest camp informat.
    #[serde(default)]
    pub telegram_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: Uuid,
    pub url: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub link_type: LinkType,
    pub tags: Vec<String>,
    pub sentiment: Sentiment,
    pub status: LinkStatus,
    pub co_reporters: Vec<Uuid>,
    /// Noms d'usuari dels reporters (resolts des de `co_reporters` per a la web).
    /// No prové d'una columna; s'omple amb `Db::fill_reporters`.
    #[serde(default)]
    pub reporters: Vec<String>,
    /// Segona passada (deep): estat, resum profund i stats de codi (repos).
    pub deep_status: DeepStatus,
    pub deep_summary: Option<String>,
    pub code_stats: Option<serde_json::Value>,
    /// Embedding quantitzat (int8) per al ranking personalitzat a la web.
    /// `e` = vector i8; `s` = factor de dequantització (f32 ≈ e[i] * s).
    #[serde(rename = "e", default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<i8>>,
    #[serde(rename = "s", default, skip_serializing_if = "Option::is_none")]
    pub embed_scale: Option<f32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Link {
    pub fn reporter_count(&self) -> usize {
        self.co_reporters.len()
    }

    /// Decideix si aquest link mereix una segona passada profunda.
    pub fn deep_applicable(&self) -> bool {
        matches!(
            self.link_type,
            LinkType::Repo | LinkType::Article | LinkType::Blog | LinkType::News | LinkType::Video
        )
    }
}

// Model de l'spec; encara no es materialitza des de la DB (vegeu reports table).
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub id: Uuid,
    pub link_id: Uuid,
    pub user_id: Uuid,
    pub status: ReportStatus,
    pub created_at: DateTime<Utc>,
}

/// Resultat de l'analisi del pipeline (LLM o fallback).
#[derive(Debug, Clone)]
pub struct Analysis {
    /// Títol curt (≤ ~80 car.) generat pel LLM; None => s'usa el títol de la pàgina.
    pub title: Option<String>,
    pub summary: String,
    pub tags: Vec<String>,
    pub sentiment: Sentiment,
}
