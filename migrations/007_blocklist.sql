-- 1) Flag per-feed: si un link d'aquesta font falla, s'esborra automàticament
--    en comptes de quedar-se en estat 'failed' i tornar a col·lectar-se.
ALTER TABLE feeds ADD COLUMN delete_on_fail INTEGER NOT NULL DEFAULT 0;

-- 2) Blocklist: patrons (regex) d'URLs que NO s'han d'acceptar mai. Es
--    comproven a report_link (camí comú de bot + feeds), sobre la URL ja
--    normalitzada.
CREATE TABLE IF NOT EXISTS blocklist (
    id TEXT PRIMARY KEY,
    pattern TEXT NOT NULL UNIQUE,
    note TEXT,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);
