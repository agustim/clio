-- Afegeix 'social' al CHECK de link_type. SQLite no permet alterar un CHECK
-- in-place: cal reconstruir la taula (crear nova, copiar, renombrar).
-- FK OFF mentre es fa el DROP de la taula referida per reports(link_id).
-- El runner executa tota la migració sobre una sola connexió.
PRAGMA foreign_keys=OFF;

CREATE TABLE links_new (
    id TEXT PRIMARY KEY,
    url TEXT UNIQUE NOT NULL,
    title TEXT,
    summary TEXT,
    link_type TEXT DEFAULT 'other' CHECK(link_type IN ('news', 'repo', 'article', 'video', 'blog', 'social', 'other')),
    tags TEXT DEFAULT '[]',
    sentiment TEXT DEFAULT 'neutral' CHECK(sentiment IN ('positive', 'neutral', 'negative')),
    status TEXT DEFAULT 'pending' CHECK(status IN ('pending', 'processing', 'done', 'failed')),
    co_reporters TEXT DEFAULT '[]',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
    deep_status TEXT DEFAULT 'none',
    deep_summary TEXT,
    code_stats TEXT,
    embedding BLOB,
    embed_scale REAL
);

INSERT INTO links_new SELECT id, url, title, summary, link_type, tags, sentiment, status, co_reporters, created_at, updated_at, deep_status, deep_summary, code_stats, embedding, embed_scale FROM links;

DROP TABLE links;

ALTER TABLE links_new RENAME TO links;

CREATE INDEX IF NOT EXISTS idx_links_status ON links(status);

CREATE INDEX IF NOT EXISTS idx_links_deep_status ON links(deep_status);

CREATE INDEX IF NOT EXISTS idx_links_embed_null ON links(id) WHERE embedding IS NULL;

PRAGMA foreign_keys=ON
