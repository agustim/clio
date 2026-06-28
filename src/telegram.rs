use crate::service::AppState;

/// Stub funcional del bot de Telegram.
///
/// Disseny previst (a implementar amb `teloxide`):
///   /start -> registra usuari + retorna api_token
///   /add <url> -> state.report_link(user, url)
///   /list -> state.db.links_reported_by(user, 5)
///   /help -> ajuda
///
/// De moment només valida la config i avisa que no està actiu.
pub async fn run(state: AppState) {
    match &state.cfg.telegram_bot_token {
        Some(_) => {
            tracing::warn!(
                "Telegram bot configurat però no implementat encara (stub). \
                 Implementar amb teloxide reutilitzant AppState::report_link."
            );
        }
        None => {
            tracing::info!("Telegram desactivat (TELEGRAM_BOT_TOKEN buit)");
        }
    }
}
