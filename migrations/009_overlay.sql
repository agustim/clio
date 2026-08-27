-- Imatge d'acompanyament per a l'overlay de directe (i futures cards):
-- og:image de l'article, o primera imatge, extreta al pipeline (1a passada).
-- Se serveix proxied via /img per evitar hotlink i exposar el referrer.
ALTER TABLE links ADD COLUMN image_url TEXT;
