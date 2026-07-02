-- NPCs (usuaris col·lectors automàtics) + feeds.
-- 1) Afegeix 'npc' al CHECK de users.role. SQLite no permet alterar un CHECK
--    in-place: cal reconstruir la taula. reports(user_id) referencia users(id),
--    per això FK OFF mentre es fa el DROP.
-- 2) Crea la taula feeds (fonts que un NPC recull periòdicament).
PRAGMA foreign_keys=OFF;

CREATE TABLE users_new (
    id TEXT PRIMARY KEY,
    username TEXT UNIQUE NOT NULL,
    api_token TEXT UNIQUE NOT NULL,
    role TEXT DEFAULT 'user' CHECK(role IN ('admin', 'user', 'npc')),
    telegram_id TEXT,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO users_new SELECT id, username, api_token, role, telegram_id, created_at FROM users;

DROP TABLE users;

ALTER TABLE users_new RENAME TO users;

CREATE TABLE IF NOT EXISTS feeds (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    kind TEXT DEFAULT 'rss' CHECK(kind IN ('rss', 'scrape')),
    source TEXT NOT NULL,
    interval_s INTEGER NOT NULL DEFAULT 3600,
    last_run TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    config_json TEXT,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(user_id, source)
);

CREATE INDEX IF NOT EXISTS idx_feeds_enabled ON feeds(enabled);

PRAGMA foreign_keys=ON
