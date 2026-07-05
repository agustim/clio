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
        // Registre de migracions aplicades (idempotent: les ALTER no es repeteixen).
        sqlx::query("CREATE TABLE IF NOT EXISTS _migrations (name TEXT PRIMARY KEY, applied_at TEXT)")
            .execute(&self.pool)
            .await?;

        let migrations: &[(&str, &str)] = &[
            ("001_init", include_str!("../migrations/001_init.sql")),
            ("002_deep", include_str!("../migrations/002_deep.sql")),
            ("003_embeddings", include_str!("../migrations/003_embeddings.sql")),
            ("004_telegram", include_str!("../migrations/004_telegram.sql")),
            ("005_social", include_str!("../migrations/005_social.sql")),
            ("006_feeds", include_str!("../migrations/006_feeds.sql")),
            ("007_blocklist", include_str!("../migrations/007_blocklist.sql")),
            ("008_fail_counts", include_str!("../migrations/008_fail_counts.sql")),
        ];

        for (name, sql) in migrations {
            let applied: Option<String> = sqlx::query("SELECT name FROM _migrations WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
                .map(|r| r.get("name"));
            if applied.is_some() {
                continue;
            }
            // Una migració s'executa sobre una sola connexió: les PRAGMA per-connexió
            // (p.ex. foreign_keys=OFF per a reconstruir taules amb FK) han de valer
            // per a tots els statements de la migració.
            let mut conn = self.pool.acquire().await?;
            for stmt in sql.split(';') {
                let s = stmt.trim();
                if s.is_empty() {
                    continue;
                }
                sqlx::query(s).execute(&mut *conn).await?;
            }
            drop(conn);
            sqlx::query("INSERT INTO _migrations (name, applied_at) VALUES (?, ?)")
                .bind(name)
                .bind(now_str())
                .execute(&self.pool)
                .await?;
            tracing::info!(migration = name, "applied");
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
            telegram_id: None,
            created_at: parse_ts(&created),
        })
    }

    const USER_COLS: &'static str = "id, username, api_token, role, telegram_id, created_at";

    fn row_to_user(r: &sqlx::sqlite::SqliteRow) -> User {
        User {
            id: parse_uuid(r.get::<String, _>("id").as_str()),
            username: r.get("username"),
            api_token: r.get("api_token"),
            role: UserRole::from_db(r.get::<String, _>("role").as_str()),
            telegram_id: r.get("telegram_id"),
            created_at: parse_ts(r.get::<String, _>("created_at").as_str()),
        }
    }

    pub async fn user_by_username(&self, username: &str) -> Result<Option<User>> {
        let q = format!("SELECT {} FROM users WHERE username = ?", Self::USER_COLS);
        let row = sqlx::query(&q).bind(username).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| Self::row_to_user(&r)))
    }

    pub async fn user_by_id(&self, id: Uuid) -> Result<Option<User>> {
        let q = format!("SELECT {} FROM users WHERE id = ?", Self::USER_COLS);
        let row = sqlx::query(&q).bind(id.to_string()).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| Self::row_to_user(&r)))
    }

    /// Cerca per id de Telegram (el bot l'usa per autoritzar qui envia links).
    pub async fn user_by_telegram_id(&self, telegram_id: &str) -> Result<Option<User>> {
        let q = format!("SELECT {} FROM users WHERE telegram_id = ?", Self::USER_COLS);
        let row = sqlx::query(&q).bind(telegram_id).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| Self::row_to_user(&r)))
    }

    pub async fn list_users(&self) -> Result<Vec<User>> {
        let q = format!("SELECT {} FROM users ORDER BY created_at", Self::USER_COLS);
        let rows = sqlx::query(&q).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(Self::row_to_user).collect())
    }

    /// Modifica nom i/o rol. Retorna l'usuari resultant (None si no existeix).
    /// Modifica nom, rol i/o telegram_id. Per a `telegram_id`: `None` = no el
    /// toquis; `Some("")` = esborra'l (NULL); `Some(x)` = posa'l.
    pub async fn update_user(
        &self,
        id: Uuid,
        username: Option<&str>,
        role: Option<UserRole>,
        telegram_id: Option<&str>,
    ) -> Result<Option<User>> {
        let Some(mut u) = self.user_by_id(id).await? else {
            return Ok(None);
        };
        if let Some(n) = username {
            u.username = n.to_string();
        }
        if let Some(r) = role {
            u.role = r;
        }
        if let Some(tid) = telegram_id {
            u.telegram_id = if tid.is_empty() { None } else { Some(tid.to_string()) };
        }
        sqlx::query("UPDATE users SET username = ?, role = ?, telegram_id = ? WHERE id = ?")
            .bind(&u.username)
            .bind(u.role.as_str())
            .bind(u.telegram_id.as_deref())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(Some(u))
    }

    /// Regenera l'api_token. Retorna el nou token (None si l'usuari no existeix).
    pub async fn regenerate_token(&self, id: Uuid) -> Result<Option<String>> {
        let token = format!("lat_{}", Uuid::new_v4().simple());
        let res = sqlx::query("UPDATE users SET api_token = ? WHERE id = ?")
            .bind(&token)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok((res.rows_affected() > 0).then_some(token))
    }

    pub async fn delete_user(&self, id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    // ---- Feeds (col·lectors NPC) ----
    const FEED_COLS: &'static str =
        "id, user_id, kind, source, interval_s, last_run, enabled, delete_on_fail, created_at";

    fn row_to_feed(r: &sqlx::sqlite::SqliteRow) -> Feed {
        let last_run: Option<String> = r.get("last_run");
        Feed {
            id: parse_uuid(r.get::<String, _>("id").as_str()),
            user_id: parse_uuid(r.get::<String, _>("user_id").as_str()),
            kind: FeedKind::from_db(r.get::<String, _>("kind").as_str()),
            source: r.get("source"),
            interval_s: r.get("interval_s"),
            last_run: last_run.as_deref().map(parse_ts),
            enabled: r.get::<i64, _>("enabled") != 0,
            delete_on_fail: r.get::<i64, _>("delete_on_fail") != 0,
            created_at: parse_ts(r.get::<String, _>("created_at").as_str()),
        }
    }

    pub async fn create_feed(
        &self,
        user_id: Uuid,
        kind: FeedKind,
        source: &str,
        interval_s: i64,
    ) -> Result<Feed> {
        let id = Uuid::new_v4();
        let created = now_str();
        sqlx::query(
            "INSERT INTO feeds (id, user_id, kind, source, interval_s, enabled, created_at) \
             VALUES (?, ?, ?, ?, ?, 1, ?)",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .bind(kind.as_str())
        .bind(source)
        .bind(interval_s)
        .bind(&created)
        .execute(&self.pool)
        .await?;
        Ok(Feed {
            id,
            user_id,
            kind,
            source: source.to_string(),
            interval_s,
            last_run: None,
            enabled: true,
            delete_on_fail: false,
            created_at: parse_ts(&created),
        })
    }

    /// Activa/desactiva l'auto-esborrat en fallada per a un feed (per source).
    /// Retorna quants feeds ha afectat.
    pub async fn set_feed_delete_on_fail(&self, source: &str, on: bool) -> Result<u64> {
        let res = sqlx::query("UPDATE feeds SET delete_on_fail = ? WHERE source = ?")
            .bind(on as i64)
            .bind(source)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Cert si el link té algun reporter que és NPC d'un feed amb
    /// `delete_on_fail` actiu (=> en fallar, s'ha d'esborrar automàticament).
    pub async fn link_from_delete_on_fail_source(&self, link_id: Uuid) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM reports r JOIN feeds f ON f.user_id = r.user_id \
             WHERE r.link_id = ? AND f.delete_on_fail = 1 LIMIT 1",
        )
        .bind(link_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Afegeix un patró a la blocklist. Error si el patró ja existeix.
    pub async fn add_block(&self, pattern: &str, note: Option<&str>) -> Result<BlockRule> {
        let id = Uuid::new_v4();
        let created = now_str();
        sqlx::query("INSERT INTO blocklist (id, pattern, note, created_at) VALUES (?, ?, ?, ?)")
            .bind(id.to_string())
            .bind(pattern)
            .bind(note)
            .bind(&created)
            .execute(&self.pool)
            .await?;
        Ok(BlockRule {
            id,
            pattern: pattern.to_string(),
            note: note.map(str::to_string),
            created_at: parse_ts(&created),
        })
    }

    /// Elimina un patró de la blocklist (per text exacte). Retorna si existia.
    pub async fn remove_block(&self, pattern: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM blocklist WHERE pattern = ?")
            .bind(pattern)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn list_blocks(&self) -> Result<Vec<BlockRule>> {
        let rows = sqlx::query("SELECT id, pattern, note, created_at FROM blocklist ORDER BY created_at")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| BlockRule {
                id: parse_uuid(r.get::<String, _>("id").as_str()),
                pattern: r.get("pattern"),
                note: r.get("note"),
                created_at: parse_ts(r.get::<String, _>("created_at").as_str()),
            })
            .collect())
    }

    /// Només els patrons (per comprovar URLs entrants a report_link).
    pub async fn blocklist_patterns(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT pattern FROM blocklist")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("pattern")).collect())
    }

    /// Incrementa el comptador de fallades consecutives d'una URL i retorna el
    /// nou valor. La fila sobreviu l'esborrat del link (font `delete_on_fail`).
    pub async fn bump_url_fail_count(&self, url: &str) -> Result<i64> {
        let count: i64 = sqlx::query(
            "INSERT INTO link_fail_counts (url, count, updated_at) VALUES (?, 1, ?) \
             ON CONFLICT(url) DO UPDATE SET count = count + 1, updated_at = excluded.updated_at \
             RETURNING count",
        )
        .bind(url)
        .bind(now_str())
        .fetch_one(&self.pool)
        .await?
        .get("count");
        Ok(count)
    }

    /// Esborra el comptador de fallades d'una URL (p.ex. en bloquejar-la o en
    /// processar-la amb èxit).
    pub async fn clear_url_fail_count(&self, url: &str) -> Result<()> {
        sqlx::query("DELETE FROM link_fail_counts WHERE url = ?")
            .bind(url)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_feeds(&self) -> Result<Vec<Feed>> {
        let q = format!("SELECT {} FROM feeds ORDER BY created_at", Self::FEED_COLS);
        let rows = sqlx::query(&q).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(Self::row_to_feed).collect())
    }

    pub async fn enabled_feeds(&self) -> Result<Vec<Feed>> {
        let q = format!("SELECT {} FROM feeds WHERE enabled = 1", Self::FEED_COLS);
        let rows = sqlx::query(&q).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(Self::row_to_feed).collect())
    }

    /// Marca l'últim intent de col·lecta (encara que falli) per no reintentar
    /// en bucle tancat.
    pub async fn touch_feed(&self, id: Uuid) -> Result<()> {
        sqlx::query("UPDATE feeds SET last_run = ? WHERE id = ?")
            .bind(now_str())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Retorna l'usuari local de la CLI, creant-lo si cal.
    pub async fn ensure_cli_user(&self) -> Result<User> {
        if let Some(u) = self.user_by_username("cli").await? {
            return Ok(u);
        }
        self.create_user("cli", UserRole::Admin).await
    }

    pub async fn user_by_token(&self, token: &str) -> Result<Option<User>> {
        let q = format!("SELECT {} FROM users WHERE api_token = ?", Self::USER_COLS);
        let row = sqlx::query(&q).bind(token).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| Self::row_to_user(&r)))
    }

    /// Resol `co_reporters` (UUIDs) -> noms d'usuari, en lot, per a una llista
    /// de links. Manté l'ordre dels reporters de cada link.
    pub async fn fill_reporters(&self, links: &mut [Link]) -> Result<()> {
        use std::collections::HashSet;
        let ids: HashSet<String> = links
            .iter()
            .flat_map(|l| l.co_reporters.iter().map(|u| u.to_string()))
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let q = format!("SELECT id, username FROM users WHERE id IN ({placeholders})");
        let mut query = sqlx::query(&q);
        let id_vec: Vec<String> = ids.into_iter().collect();
        for id in &id_vec {
            query = query.bind(id);
        }
        let rows = query.fetch_all(&self.pool).await?;
        let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for r in &rows {
            names.insert(r.get::<String, _>("id"), r.get::<String, _>("username"));
        }
        for l in links.iter_mut() {
            l.reporters = l
                .co_reporters
                .iter()
                .filter_map(|u| names.get(&u.to_string()).cloned())
                .collect();
        }
        Ok(())
    }

    // ---- Links ----

    fn row_to_link(r: &sqlx::sqlite::SqliteRow) -> Result<Link> {
        let tags_json: String = r.get("tags");
        let cor_json: String = r.get("co_reporters");
        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        let cor_strs: Vec<String> = serde_json::from_str(&cor_json).unwrap_or_default();
        let co_reporters = cor_strs.iter().map(|s| parse_uuid(s)).collect();
        let deep_summary: Option<String> = r.get("deep_summary");
        let code_stats_raw: Option<String> = r.get("code_stats");
        let code_stats = code_stats_raw
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        // BLOB de bytes -> Vec<i8> (reinterpretació directa).
        let embedding: Option<Vec<i8>> = r
            .get::<Option<Vec<u8>>, _>("embedding")
            .map(|b| b.into_iter().map(|x| x as i8).collect());
        let embed_scale: Option<f32> =
            r.get::<Option<f64>, _>("embed_scale").map(|s| s as f32);
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
            reporters: Vec::new(), // s'omple amb fill_reporters
            deep_status: DeepStatus::from_db(
                r.get::<Option<String>, _>("deep_status").as_deref().unwrap_or("none"),
            ),
            deep_summary,
            code_stats,
            embedding,
            embed_scale,
            created_at: parse_ts(r.get::<String, _>("created_at").as_str()),
            updated_at: parse_ts(r.get::<String, _>("updated_at").as_str()),
        })
    }

    const LINK_COLS: &'static str = "id, url, title, summary, link_type, tags, sentiment, status, co_reporters, deep_status, deep_summary, code_stats, embedding, embed_scale, created_at, updated_at";

    pub async fn link_by_url(&self, url: &str) -> Result<Option<Link>> {
        let q = format!("SELECT {} FROM links WHERE url = ?", Self::LINK_COLS);
        let row = sqlx::query(&q).bind(url).fetch_optional(&self.pool).await?;
        row.map(|r| Self::row_to_link(&r)).transpose()
    }

    pub async fn link_by_id(&self, id: Uuid) -> Result<Option<Link>> {
        let q = format!("SELECT {} FROM links WHERE id = ?", Self::LINK_COLS);
        let row = sqlx::query(&q).bind(id.to_string()).fetch_optional(&self.pool).await?;
        let mut link = row.map(|r| Self::row_to_link(&r)).transpose()?;
        if let Some(l) = link.as_mut() {
            self.fill_reporters(std::slice::from_mut(l)).await?;
        }
        Ok(link)
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

    /// Esborra un link i els seus reports. Retorna true si existia.
    pub async fn delete_link(&self, link_id: Uuid) -> Result<bool> {
        sqlx::query("DELETE FROM reports WHERE link_id = ?")
            .bind(link_id.to_string())
            .execute(&self.pool)
            .await?;
        let res = sqlx::query("DELETE FROM links WHERE id = ?")
            .bind(link_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
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

    // ---- Deep pass ----

    pub async fn set_deep_status(&self, link_id: Uuid, status: DeepStatus) -> Result<()> {
        sqlx::query("UPDATE links SET deep_status = ?, updated_at = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(now_str())
            .bind(link_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_deep_analysis(
        &self,
        link_id: Uuid,
        deep_summary: &str,
        code_stats: Option<&serde_json::Value>,
    ) -> Result<()> {
        let stats = code_stats.map(serde_json::to_string).transpose()?;
        sqlx::query(
            "UPDATE links SET deep_summary = ?, code_stats = ?, deep_status = 'done', \
             updated_at = ? WHERE id = ?",
        )
        .bind(deep_summary)
        .bind(stats)
        .bind(now_str())
        .bind(link_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- Embeddings ----

    /// Desa l'embedding quantitzat (int8) i el seu factor d'escala.
    pub async fn update_link_embedding(
        &self,
        link_id: Uuid,
        embedding: &[i8],
        scale: f32,
    ) -> Result<()> {
        // i8 -> bytes per al BLOB.
        let bytes: Vec<u8> = embedding.iter().map(|&x| x as u8).collect();
        sqlx::query("UPDATE links SET embedding = ?, embed_scale = ? WHERE id = ?")
            .bind(bytes)
            .bind(scale as f64)
            .bind(link_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Tots els ids de links (més recents primer), per a reprocessament massiu.
    pub async fn all_link_ids(&self, limit: i64) -> Result<Vec<Uuid>> {
        let rows = sqlx::query("SELECT id FROM links ORDER BY updated_at DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| parse_uuid(r.get::<String, _>("id").as_str())).collect())
    }

    /// Links ja processats (shallow done) però sense embedding: per al backfill.
    pub async fn missing_embedding_ids(&self) -> Result<Vec<Uuid>> {
        let rows = sqlx::query(
            "SELECT id FROM links WHERE status = 'done' AND embedding IS NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| parse_uuid(r.get::<String, _>("id").as_str())).collect())
    }

    // ---- Recovery (re-encua feina pendent en arrencar) ----

    /// Links amb shallow pendent/encallat o fallit.
    pub async fn pending_shallow_ids(&self) -> Result<Vec<Uuid>> {
        let rows = sqlx::query(
            "SELECT id FROM links WHERE status IN ('pending','processing','failed')",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| parse_uuid(r.get::<String, _>("id").as_str())).collect())
    }

    /// Links amb shallow fet però deep pendent/encallat.
    pub async fn pending_deep_ids(&self) -> Result<Vec<Uuid>> {
        let rows = sqlx::query(
            "SELECT id FROM links WHERE status = 'done' AND deep_status IN ('pending','processing')",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| parse_uuid(r.get::<String, _>("id").as_str())).collect())
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
        let mut links: Vec<Link> = rows.iter().map(Self::row_to_link).collect::<Result<_>>()?;
        self.fill_reporters(&mut links).await?;
        Ok(links)
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
