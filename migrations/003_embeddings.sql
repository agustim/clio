-- Embeddings per link: vector semàntic quantitzat a int8 (BLOB) + factor d'escala.
-- Permet ranking personalitzat client-side (cosine vs centroide de "cors"), sense usuaris.
ALTER TABLE links ADD COLUMN embedding BLOB;       -- bytes int8 (i8 com a u8), embed_dim elements
ALTER TABLE links ADD COLUMN embed_scale REAL;     -- dequant: f32 ≈ i8 * embed_scale

CREATE INDEX IF NOT EXISTS idx_links_embed_null ON links(id) WHERE embedding IS NULL;
