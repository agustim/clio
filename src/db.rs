use crate::error::{AppError, Result};
use crate::models::*;
use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;
use uuid::Uuid;

const TS_FMT: &str = "%Y-%m-%d %H:%M:%S";

pub fn now_str() -> String {
    Utc::now().format(TS_FMT).to_string()
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    // SQLite CURRENT_TIMESTAMP => "YYYY-MM-DD HH:MM:SS" (UTC). Tolerant amb fraccions.
    let trimmed = s.split('.').next().unwrap_or(s);
    NaiveDateTime::parse_from_str(trimmed, TS_FMT)
        .map(|n| n.and_utc())
        .unwrap_or_else(|_| Utc::now())
}

fn parse_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap_or_else(|_| Uuid::nil())
}

#[derive(Clone)]
pub struct Db {
    pub pool: SqlitePool,
}

impl Db {
    pub async fn connect(database_url: &str) -> Result<Self> {
        // Assegura el directori del fitxer sqlite.
        if let Some(path) = database_url.strip_prefix("sqlite://") {
            if let Some(parent) = std::path::Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
        }
        let opts = SqliteConnectOptions::from_str(database_url)
            .map_err(|e| AppError::Config(format!("bad DATABASE_URL: {e}")))?
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        let db = Db { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<()> {
        let sql = include_str!("../migrations/001_init.sql");
        // Executa cada statement separat per ';'.
        for stmt in sql.split(';') {
            let s = stmt.trim();
            if s.is_empty() {
                continue;
            }
            sqlx::query(s).execute(&self.pool).await?;
        }
        Ok(())
    }

    // ---- Users ----

    pub async fn create_user(&self, username: &str, role: UserRole) -> Result<User> {
        let id = Uuid::new_v4();
        let token = format!("lat_{}", Uuid::new_v4().simple());
        let created = now_str();
        sqlx::query(
            "INSERT INTO users (id, username, api_token, role, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(username)
        .bind(&token)
        .bind(role.as_str())
        .bind(&created)
        .execute(&self.pool)
        .await?;
        Ok(User {
            id,
            username: username.to_string(),
            api_token: token,
            role,
            created_at: parse_ts(&created),
        })
    }

    pub async fn user_by_username(&self, username: &str) -> Result<Option<User>> {
        let row = sqlx::query("SELECT id, username, api_token, role, created_at FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| User {
            id: parse_uuid(r.get::<String, _>("id").as_str()),
            username: r.get("username"),
            api_token: r.get("api_token"),
            role: UserRole::from_db(r.get::<String, _>("role").as_str()),
            created_at: parse_ts(r.get::<String, _>("created_at").as_str()),
        }))
    }

    /// Retorna l'usuari local de la CLI, creant-lo si cal.
    pub async fn ensure_cli_user(&self) -> Result<User> {
        if let Some(u) = self.user_by_username("cli").await? {
            return Ok(u);
        }
        self.create_user("cli", UserRole::Admin).await
    }

    pub async fn user_by_token(&self, token: &str) -> Result<Option<User>> {
        let row = sqlx::query("SELECT id, username, api_token, role, created_at FROM users WHERE api_token = ?")
            .bind(token)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| User {
            id: parse_uuid(r.get::<String, _>("id").as_str()),
            username: r.get("username"),
            api_token: r.get("api_token"),
            role: UserRole::from_db(r.get::<String, _>("role").as_str()),
            created_at: parse_ts(r.get::<String, _>("created_at").as_str()),
        }))
    }

    // ---- Links ----

    fn row_to_link(r: &sqlx::sqlite::SqliteRow) -> Result<Link> {
        let tags_json: String = r.get("tags");
        let cor_json: String = r.get("co_reporters");
        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        let cor_strs: Vec<String> = serde_json::from_str(&cor_json).unwrap_or_default();
        let co_reporters = cor_strs.iter().map(|s| parse_uuid(s)).collect();
        Ok(Link {
            id: parse_uuid(r.get::<String, _>("id").as_str()),
            url: r.get("url"),
            title: r.get("title"),
            summary: r.get("summary"),
            link_type: LinkType::from_db(r.get::<String, _>("link_type").as_str()),
            tags,
            sentiment: Sentiment::from_db(r.get::<String, _>("sentiment").as_str()),
            status: LinkStatus::from_db(r.get::<String, _>("status").as_str()),
            co_reporters,
            created_at: parse_ts(r.get::<String, _>("created_at").as_str()),
            updated_at: parse_ts(r.get::<String, _>("updated_at").as_str()),
        })
    }

    const LINK_COLS: &'static str = "id, url, title, summary, link_type, tags, sentiment, status, co_reporters, created_at, updated_at";

    pub async fn link_by_url(&self, url: &str) -> Result<Option<Link>> {
        let q = format!("SELECT {} FROM links WHERE url = ?", Self::LINK_COLS);
        let row = sqlx::query(&q).bind(url).fetch_optional(&self.pool).await?;
        row.map(|r| Self::row_to_link(&r)).transpose()
    }

    pub async fn link_by_id(&self, id: Uuid) -> Result<Option<Link>> {
        let q = format!("SELECT {} FROM links WHERE id = ?", Self::LINK_COLS);
        let row = sqlx::query(&q).bind(id.to_string()).fetch_optional(&self.pool).await?;
        row.map(|r| Self::row_to_link(&r)).transpose()
    }

    /// Crea link nou amb un primer reporter. Estat = pending.
    pub async fn create_link(&self, url: &str, reporter: Uuid) -> Result<Link> {
        let id = Uuid::new_v4();
        let now = now_str();
        let cor = serde_json::to_string(&vec![reporter.to_string()])?;
        sqlx::query(
            "INSERT INTO links (id, url, tags, co_reporters, status, created_at, updated_at) \
             VALUES (?, ?, '[]', ?, 'pending', ?, ?)",
        )
        .bind(id.to_string())
        .bind(url)
        .bind(&cor)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        self.link_by_id(id).await?.ok_or(AppError::NotFound)
    }

    /// Afegeix un co-reporter si encara no hi es. Retorna true si afegit.
    pub async fn add_co_reporter(&self, link_id: Uuid, user_id: Uuid) -> Result<bool> {
        let link = self.link_by_id(link_id).await?.ok_or(AppError::NotFound)?;
        if link.co_reporters.contains(&user_id) {
            return Ok(false);
        }
        let mut list: Vec<String> = link.co_reporters.iter().map(|u| u.to_string()).collect();
        list.push(user_id.to_string());
        let json = serde_json::to_string(&list)?;
        sqlx::query("UPDATE links SET co_reporters = ?, updated_at = ? WHERE id = ?")
            .bind(json)
            .bind(now_str())
            .bind(link_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    pub async fn set_link_status(&self, link_id: Uuid, status: LinkStatus) -> Result<()> {
        sqlx::query("UPDATE links SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(now_str())
            .bind(link_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_link_analysis(
        &self,
        link_id: Uuid,
        title: Option<&str>,
        link_type: LinkType,
        analysis: &Analysis,
    ) -> Result<()> {
        let tags = serde_json::to_string(&analysis.tags)?;
        sqlx::query(
            "UPDATE links SET title = ?, summary = ?, link_type = ?, tags = ?, sentiment = ?, \
             status = 'done', updated_at = ? WHERE id = ?",
        )
        .bind(title)
        .bind(&analysis.summary)
        .bind(link_type.as_str())
        .bind(tags)
        .bind(analysis.sentiment.as_str())
        .bind(now_str())
        .bind(link_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- Reports ----

    /// Insereix report (ignora si duplicat per UNIQUE(link_id,user_id)).
    pub async fn add_report(&self, link_id: Uuid, user_id: Uuid) -> Result<()> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT OR IGNORE INTO reports (id, link_id, user_id, status, created_at) \
             VALUES (?, ?, ?, 'pending', ?)",
        )
        .bind(id.to_string())
        .bind(link_id.to_string())
        .bind(user_id.to_string())
        .bind(now_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- Queries ----

    pub async fn list_links(
        &self,
        tag: Option<&str>,
        sentiment: Option<Sentiment>,
        link_type: Option<LinkType>,
        limit: i64,
    ) -> Result<Vec<Link>> {
        let mut q = format!("SELECT {} FROM links WHERE 1=1", Self::LINK_COLS);
        if tag.is_some() {
            q.push_str(" AND tags LIKE ?");
        }
        if sentiment.is_some() {
            q.push_str(" AND sentiment = ?");
        }
        if link_type.is_some() {
            q.push_str(" AND link_type = ?");
        }
        q.push_str(" ORDER BY updated_at DESC LIMIT ?");

        let mut query = sqlx::query(&q);
        if let Some(t) = tag {
            query = query.bind(format!("%\"{}\"%", t.to_lowercase()));
        }
        if let Some(s) = sentiment {
            query = query.bind(s.as_str());
        }
        if let Some(lt) = link_type {
            query = query.bind(lt.as_str());
        }
        query = query.bind(limit);

        let rows = query.fetch_all(&self.pool).await?;
        rows.iter().map(Self::row_to_link).collect()
    }

    /// Usat pel comando /list del bot Telegram (encara stub).
    #[allow(dead_code)]
    pub async fn links_reported_by(&self, user_id: Uuid, limit: i64) -> Result<Vec<Link>> {
        let q = format!(
            "SELECT {} FROM links l JOIN reports r ON r.link_id = l.id \
             WHERE r.user_id = ? ORDER BY r.created_at DESC LIMIT ?",
            Self::LINK_COLS
                .split(", ")
                .map(|c| format!("l.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let rows = sqlx::query(&q)
            .bind(user_id.to_string())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Self::row_to_link).collect()
    }

    pub async fn stats(&self) -> Result<Stats> {
        let total: i64 = sqlx::query("SELECT COUNT(*) AS c FROM links")
            .fetch_one(&self.pool)
            .await?
            .get("c");
        let done: i64 = sqlx::query("SELECT COUNT(*) AS c FROM links WHERE status='done'")
            .fetch_one(&self.pool)
            .await?
            .get("c");
        let pending: i64 = sqlx::query("SELECT COUNT(*) AS c FROM links WHERE status IN ('pending','processing')")
            .fetch_one(&self.pool)
            .await?
            .get("c");
        let users: i64 = sqlx::query("SELECT COUNT(*) AS c FROM users")
            .fetch_one(&self.pool)
            .await?
            .get("c");
        Ok(Stats { total_links: total, done, pending, users })
    }
}

#[derive(Debug, serde::Serialize)]
pub struct Stats {
    pub total_links: i64,
    pub done: i64,
    pub pending: i64,
    pub users: i64,
}
