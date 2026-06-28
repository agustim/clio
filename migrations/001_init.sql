-- users table
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,          -- UUID v4
    username TEXT UNIQUE NOT NULL,
    api_token TEXT UNIQUE NOT NULL,
    role TEXT DEFAULT 'user' CHECK(role IN ('admin', 'user')),
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

-- links table (Core entity)
CREATE TABLE IF NOT EXISTS links (
    id TEXT PRIMARY KEY,          -- UUID v4
    url TEXT UNIQUE NOT NULL,     -- URL normalitzada
    title TEXT,
    summary TEXT,
    link_type TEXT DEFAULT 'other' CHECK(link_type IN ('news', 'repo', 'article', 'video', 'blog', 'other')),
    tags TEXT DEFAULT '[]',
    sentiment TEXT DEFAULT 'neutral' CHECK(sentiment IN ('positive', 'neutral', 'negative')),
    status TEXT DEFAULT 'pending' CHECK(status IN ('pending', 'processing', 'done', 'failed')),
    co_reporters TEXT DEFAULT '[]',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT DEFAULT CURRENT_TIMESTAMP
);

-- reports table (Audit trail & Linking)
CREATE TABLE IF NOT EXISTS reports (
    id TEXT PRIMARY KEY,
    link_id TEXT REFERENCES links(id),
    user_id TEXT REFERENCES users(id),
    status TEXT DEFAULT 'pending',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(link_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_links_status ON links(status);
CREATE INDEX IF NOT EXISTS idx_reports_user ON reports(user_id);
