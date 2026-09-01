-- MP3 de la veu del titular (TTS Edge + transform), generat en analitzar la
-- cua. Fitxer dins de TTS_DIR (p.ex. data/tts/<link_id>.mp3), servit a
-- /audio/{id}. Buit/null = l'overlay no en té veu per a aquesta notícia.
ALTER TABLE links ADD COLUMN audio_file TEXT;
