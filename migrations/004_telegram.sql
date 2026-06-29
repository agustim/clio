-- Identificador de Telegram de l'usuari (per acceptar links del bot).
ALTER TABLE users ADD COLUMN telegram_id TEXT;
CREATE INDEX IF NOT EXISTS idx_users_telegram_id ON users(telegram_id);
