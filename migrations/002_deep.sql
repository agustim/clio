-- Segona passada (deep): anàlisi profunda asíncrona.
ALTER TABLE links ADD COLUMN deep_status TEXT DEFAULT 'none';
ALTER TABLE links ADD COLUMN deep_summary TEXT;
ALTER TABLE links ADD COLUMN code_stats TEXT; -- JSON: estadístiques de codi (repos)

CREATE INDEX IF NOT EXISTS idx_links_deep_status ON links(deep_status);
