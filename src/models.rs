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
    Video => "video", Blog => "blog", Other => "other"
}, default = Other);
str_enum!(Sentiment {
    Positive => "positive", Neutral => "neutral", Negative => "negative"
}, default = Neutral);
str_enum!(LinkStatus {
    Pending => "pending", Processing => "processing", Done => "done", Failed => "failed"
}, default = Pending);
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
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Link {
    pub fn reporter_count(&self) -> usize {
        self.co_reporters.len()
    }
}

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
    pub summary: String,
    pub tags: Vec<String>,
    pub sentiment: Sentiment,
}
