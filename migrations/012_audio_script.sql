-- Text parlat de la notícia: titular + resum de 40-50 paraules generat en
-- acabar l'anàlisi profunda (que és quan la notícia pot aparèixer a l'overlay).
-- És el text que sintetitza TTS a data/tts/<link_id>.mp3 (/audio/{id}).
-- Es desa perquè el backfill de veu no hagi de tornar a cridar el LLM.
ALTER TABLE links ADD COLUMN audio_script TEXT;
