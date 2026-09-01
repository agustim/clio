mod api;
mod cli;
mod config;
mod db;
mod deep;
mod embed;
mod error;
mod feeds;
mod flaresolverr;
mod llm;
mod models;
mod normalize;
mod overlay;
mod pipeline;
mod queue;
mod service;
mod social;
mod telegram;
mod voice;
mod webgen;

use clap::Parser;
use config::Config;
use db::Db;
use service::AppState;
use tracing_subscriber::EnvFilter;

/// Carrega el `.env` de la carpeta actual (o dels pares) cap el procés,
/// **sense sobreescriure** variables ja definides (així el CLI/parent pot
/// fer override). Parser propi, mínim i robust: NO fa seguiment de cometes
/// ni escapes, de manera que un apòstrof en un comentari (p. ex. `l'escena`)
/// no pot trencar el parseig — cosa que passava amb `dotenvy` 0.15, que s'acabava
/// empassant les línies següents (MAX_ITEMS i CARDS quedaven sense carregar).
/// Sovint n'hi ha prou amb la primera línia no buida: la resta dels pares s'ignora.
fn load_local_env() {
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let candidate = d.join(".env");
        if candidate.is_file() {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                let n = apply_env_file(&content);
                tracing::debug!("cartoga .env: {} ({} variables aplicades)", candidate.display(), n);
                return;
            }
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
    tracing::debug!("cap .env trobat (la configuració ve només de l'entorn)");
}

/// Aplica un contingut de `.env` a l'entorn actual. Retorna quantes variables
/// s'han definit realment. Regles simples:
/// - línies buides o que comencin per `#` es descarten (opcional `export `).
/// - clau: text alfanumèric/`_`/`.` abans del primer `=`.
/// - valor: text després del `=`; es tallen cometes dobles/simples senceres i
///   es retalla un comentari quan el `#` va precedit de blanc.
/// - NO se sobreescriuen variables ja presents a l'entorn.
fn apply_env_file(content: &str) -> usize {
    let mut n = 0;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let mut value = line[eq + 1..].trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        {
            continue;
        }
        // Comentari dins el valor (només si el `#` va precedit de blanc):
        if let Some(hi) = value.find('#') {
            let before = value.as_bytes().get(hi.wrapping_sub(1)).copied();
            if !before.is_some_and(|b| b == b' ' || b == b'\t') {
                // `k=v#x` sense blanc: el `#` és part del valor
            } else {
                value = value[..hi].trim();
            }
        }
        // Cometes senceres al valor:
        if value.len() >= 2 {
            let b = value.as_bytes();
            if (b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\'')
            {
                value = &value[1..value.len() - 1];
            }
        }
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod env_tests {
    use super::apply_env_file;

    // Esborra claus controlades per no contaminar l'entorn del test runner.
    fn reset(keys: &[&str]) {
        for k in keys {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn comentaris_amb_apostrof_no_trenquen() {
        reset(&["OVERLAY_ROTATE_SECS", "OVERLAY_MAX_ITEMS", "OVERLAY_CARDS"]);
        let src = "OVERLAY_ROTATE_SECS=15    # cada quants segons roten les cards de l'escena\nOVERLAY_MAX_ITEMS=50      # quantes notícies entren al ticker/crawl\nOVERLAY_CARDS=3           # quantes cards visibles a l'escena\n";
        let n = apply_env_file(src);
        assert_eq!(n, 3, "totes tres variables s'han d'aplicar malgrat els apòstrofs");
        assert_eq!(std::env::var("OVERLAY_ROTATE_SECS").unwrap(), "15");
        assert_eq!(std::env::var("OVERLAY_MAX_ITEMS").unwrap(), "50");
        assert_eq!(std::env::var("OVERLAY_CARDS").unwrap(), "3");
        reset(&["OVERLAY_ROTATE_SECS", "OVERLAY_MAX_ITEMS", "OVERLAY_CARDS"]);
    }

    #[test]
    fn fragment_representatiu_amb_molts_apostrofs() {
        // Simula l'estil real del nostre .env (comentaris en català amb ' )
        // per seguir el cas del bug: cap línia s'ha de quedar per carregar.
        reset(&["OVERLAY_REFRESH_SECS", "OVERLAY_TEXT_LINES", "OVERLAY_TIMEZONE", "TTS_DIR", "TTS_RATE_FACTOR"]);
        let src = "OVERLAY_REFRESH_SECS=60   # cada quants segons es re-carrega el ticker\n\
                   OVERLAY_TEXT_LINES=9     # línies de text LLM visibles per card (triplicat per defecte)\n\
                   OVERLAY_TIMEZONE=Europe/Andorra\n\
                   TTS_DIR=data/tts\n\
                   TTS_RATE_FACTOR=0.75\n";
        assert_eq!(apply_env_file(src), 5);
        assert_eq!(std::env::var("OVERLAY_REFRESH_SECS").unwrap(), "60");
        assert_eq!(std::env::var("OVERLAY_TEXT_LINES").unwrap(), "9");
        assert_eq!(std::env::var("OVERLAY_TIMEZONE").unwrap(), "Europe/Andorra");
        assert_eq!(std::env::var("TTS_DIR").unwrap(), "data/tts");
        assert_eq!(std::env::var("TTS_RATE_FACTOR").unwrap(), "0.75");
        reset(&["OVERLAY_REFRESH_SECS", "OVERLAY_TEXT_LINES", "OVERLAY_TIMEZONE", "TTS_DIR", "TTS_RATE_FACTOR"]);
    }

    #[test]
    fn valors_amb_cometes_i_commentari_dins() {
        reset(&["A", "B", "C", "E", "F"]);
        assert_eq!(apply_env_file("A=\"hola món\"\nB='deia'\nC=1\n"), 3);
        assert_eq!(std::env::var("A").unwrap(), "hola món");
        assert_eq!(std::env::var("B").unwrap(), "deia");
        assert_eq!(std::env::var("C").unwrap(), "1");
        // `#` sense blanc NO talla el valor; amb blanc sí:
        assert_eq!(apply_env_file("E=foo#bar\nF=baz # comentari\n"), 2);
        assert_eq!(std::env::var("E").unwrap(), "foo#bar");
        assert_eq!(std::env::var("F").unwrap(), "baz");
        reset(&["A", "B", "C", "E", "F"]);
    }

    #[test]
    fn no_sobreescriu_variables_ja_definides() {
        reset(&["EXISTEIX"]);
        std::env::set_var("EXISTEIX", "entorn");
        assert_eq!(apply_env_file("EXISTEIX=cau\n"), 0);
        assert_eq!(std::env::var("EXISTEIX").unwrap(), "entorn");
        reset(&["EXISTEIX"]);
    }
}

#[tokio::main]
async fn main() {
    load_local_env();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    if let Err(e) = run().await {
        tracing::error!("fatal: {e}");
        std::process::exit(1);
    }
}

async fn run() -> error::Result<()> {
    let args = cli::Cli::parse();
    let cfg = Config::from_env()?;
    let db = Db::connect(&cfg.database_url).await?;
    let (state, rx) = AppState::new(db, cfg)?;
    cli::run(state, rx, args.command).await
}
