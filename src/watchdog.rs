//! Watchdog del model: sondeja periòdicament l'endpoint del LLM i avisa per
//! Telegram quan deixa de respondre i quan torna.
//!
//! Per què cal: si el model cau en un moment tranquil, sense trànsit no es
//! detecta. El circuit breaker només s'obre després de vuit fallades reals
//! (o sigui, vuit links), cosa que pot trigar hores. La sonda va per fora
//! d'aquest camí: no toca el circuit ni el rate limiter (`LlmClient::probe`).
//!
//! Regles: un sol avís de baixada per caiguda (quan se superen
//! `LLM_WATCH_FAILS` sondes fallides seguides) i un sol avís de pujada en
//! recuperar-se. Així no es repeteix el missatge mentre dura la caiguda.

use crate::service::AppState;
use std::time::{Duration, Instant};

/// Transició detectada per la sonda.
#[derive(Debug, PartialEq, Eq)]
pub enum Transition {
    /// El model ha deixat de respondre.
    Down,
    /// El model torna a respondre, després del temps que ha estat caigut.
    Up(Duration),
}

/// Màquina d'estats de la sonda (sense I/O, per poder-la testejar).
struct Watch {
    /// Sondes fallides seguides.
    fails: u32,
    /// Fallides seguides que calen per declarar la caiguda.
    threshold: u32,
    /// Si ara mateix considerem el model caigut.
    down: bool,
    /// Instant en què es va declarar la caiguda.
    down_since: Option<Instant>,
}

impl Watch {
    fn new(threshold: u32) -> Self {
        Self {
            fails: 0,
            threshold: threshold.max(1),
            down: false,
            down_since: None,
        }
    }

    fn is_down(&self) -> bool {
        self.down
    }

    /// Registra el resultat d'una sonda i retorna la transició, si n'hi ha.
    ///
    /// Un cop declarada la baixada, les fallides següents no repeteixen
    /// l'avís; un cop recuperat, els èxits següents tampoc.
    fn observe(&mut self, ok: bool, now: Instant) -> Option<Transition> {
        if ok {
            self.fails = 0;
            if self.down {
                self.down = false;
                let since = self.down_since.take().unwrap_or(now);
                return Some(Transition::Up(now.saturating_duration_since(since)));
            }
            return None;
        }
        self.fails += 1;
        if !self.down && self.fails >= self.threshold {
            self.down = true;
            self.down_since = Some(now);
            return Some(Transition::Down);
        }
        None
    }
}

/// Bucle de fons (spawn a `serve`): sondeja el model i notifica les
/// transicions per Telegram.
pub async fn run(state: AppState) {
    let Some(llm) = state.llm.clone() else {
        tracing::info!("watchdog del model: sense LLM configurat, no s'activa");
        return;
    };
    let secs = state.cfg.llm_watch_secs;
    if secs == 0 {
        tracing::info!("watchdog del model: desactivat (LLM_WATCH_SECS=0)");
        return;
    }
    let interval = Duration::from_secs(secs);
    let mut watch = Watch::new(state.cfg.llm_watch_fails);
    tracing::info!(
        secs,
        fails = state.cfg.llm_watch_fails,
        "watchdog del model actiu"
    );

    loop {
        let result = llm.probe().await;
        match watch.observe(result.is_ok(), Instant::now()) {
            Some(Transition::Down) => {
                let err = result.err().unwrap_or_default();
                tracing::warn!(error = %err, "watchdog: el model no respon");
                state.notify(&down_message(&state, &err, secs)).await;
            }
            Some(Transition::Up(elapsed)) => {
                tracing::info!(
                    secs = elapsed.as_secs(),
                    "watchdog: el model torna a respondre"
                );
                state.notify(&up_message(elapsed)).await;
            }
            None => {}
        }
        tokio::time::sleep(interval).await;
    }
}

/// Avís de caiguda: model, endpoint, últim error i què passa mentrestant.
fn down_message(state: &AppState, err: &str, secs: u64) -> String {
    format!(
        "🔴 Clio: no puc accedir al model\n\
         {} · {}\n\
         {}\n\
         L'anàlisi de links queda aturada (els nous quedaran en 'failed'). \
         Torno a provar cada {secs}s i t'aviso quan respongui.",
        state.cfg.llm.model, state.cfg.llm.base_url, err
    )
}

/// Avís de recuperació, amb el temps que ha durat la caiguda.
fn up_message(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let human = if secs >= 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    };
    format!(
        "🟢 Clio: el model torna a respondre (caigut {human}). \
         Reprenc l'anàlisi i el reintent dels links fallits."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avisa_un_sol_cop_per_caiguda_i_per_recuperacio() {
        let t0 = Instant::now();
        let mut w = Watch::new(3);
        assert_eq!(w.observe(false, t0), None);
        assert_eq!(w.observe(false, t0), None);
        assert_eq!(w.observe(false, t0), Some(Transition::Down));
        assert!(w.is_down());
        // Més fallides: cap avís repetit.
        assert_eq!(w.observe(false, t0), None);
        assert_eq!(w.observe(false, t0), None);
        // Recuperació: un sol avís, amb la durada.
        assert_eq!(
            w.observe(true, t0 + Duration::from_secs(125)),
            Some(Transition::Up(Duration::from_secs(125)))
        );
        assert!(!w.is_down());
        assert_eq!(w.observe(true, t0 + Duration::from_secs(190)), None);
    }

    #[test]
    fn una_fallada_aillada_no_avisa() {
        let t0 = Instant::now();
        let mut w = Watch::new(3);
        assert_eq!(w.observe(false, t0), None);
        assert_eq!(w.observe(true, t0), None); // es reinicia el comptador
        assert_eq!(w.observe(false, t0), None);
        assert_eq!(w.observe(false, t0), None);
        assert!(!w.is_down());
    }

    #[test]
    fn pot_tornar_a_avisar_en_una_segona_caiguda() {
        let t0 = Instant::now();
        let mut w = Watch::new(1);
        assert_eq!(w.observe(false, t0), Some(Transition::Down));
        assert_eq!(w.observe(true, t0), Some(Transition::Up(Duration::ZERO)));
        assert_eq!(w.observe(false, t0), Some(Transition::Down));
        assert_eq!(w.observe(true, t0), Some(Transition::Up(Duration::ZERO)));
    }

    #[test]
    fn llindar_zero_es_tracta_com_a_u() {
        let t0 = Instant::now();
        let mut w = Watch::new(0);
        assert_eq!(w.observe(false, t0), Some(Transition::Down));
    }

    #[test]
    fn recuperar_se_sense_caiguda_no_avisa() {
        let t0 = Instant::now();
        let mut w = Watch::new(3);
        assert_eq!(w.observe(true, t0), None);
    }
}
