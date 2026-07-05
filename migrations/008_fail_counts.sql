-- Comptador de fallades consecutives per URL. Els links de fonts amb
-- `delete_on_fail` s'esborren en fallar, però la font els torna a col·lectar;
-- per detectar-ne un que sempre falla necessitem un comptador que sobrevisqui
-- l'esborrat del link. Quan arriba a NUM_ERRORS_TO_BLACKLIST, la URL entra a la
-- blocklist i deixa d'acceptar-se.
CREATE TABLE IF NOT EXISTS link_fail_counts (
    url TEXT PRIMARY KEY,
    count INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT DEFAULT CURRENT_TIMESTAMP
);
