-- Còpia local de la imatge d'acompanyament (fitxer dins d'IMAGES_DIR, p.ex.
-- data/images/). Buit/null = l'overlay fa servir el proxy remot /img?u=.
ALTER TABLE links ADD COLUMN image_file TEXT;
