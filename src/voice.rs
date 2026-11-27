//! Veu de les notícies (TTS Edge + transform en Rust via `clio-voice`).
//!
//! En acabar l'anàlisi profunda (el moment en què una notícia pot aparèixer a
//! l'overlay del directe) es genera el **text parlat** d'aquesta notícia:
//! el titular seguit d'un **resum de 40-50 paraules** que en faci entendre el
//! contingut. El resum l'escriu el LLM a partir de `deep_summary`; si el model
//! falla o no n'hi ha, es fa una extracció automàtica de les primeres frases.
//!
//! El text es desa a `links.audio_script` i se sintetitza a
//! `TTS_DIR/{link_id}.mp3` (nom a `links.audio_file`). L'overlay el serveix a
//! `/audio/{id}` i el llegeix dins del grup de cards.
//!
//! Tot es *best-effort*: mai fa fallar l'anàlisi d'un link (si el LLM o la
//! sintesi cauen, el link queda igualment `done` i només s'escriu un warning).

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;
use crate::llm::LlmClient;
use crate::models::{Link, LinkType};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Marge de paraules respecte `TTS_SCRIPT_WORDS` que acceptem del LLM. Fora
/// d'aquest marge es descarta la resposta i es fa l'extracció automàtica: un
/// text massa curt no explica la notícia i un de massa llarg allarga el directe.
const SCRIPT_WORD_TOLERANCE: usize = 15;

/// Un tipus de link "noticiós" (els que l'overlay llegeix en veu alta).
pub fn is_newsy(link_type: LinkType) -> bool {
    matches!(
        link_type,
        LinkType::News | LinkType::Article | LinkType::Blog | LinkType::Video
    )
}

/// Cal generar veu per a aquest tipus segons la config?
pub fn applies(cfg: &Config, link_type: LinkType) -> bool {
    if !cfg.tts.enabled() {
        return false;
    }
    if !cfg.tts.only_news {
        return true;
    }
    is_newsy(link_type)
}

/// Converteix la config de Clio a la del crate de veu.
pub fn voice_config(cfg: &Config) -> clio_voice::VoiceConfig {
    clio_voice::VoiceConfig {
        voice: cfg.tts.voice.clone(),
        lang: cfg.tts.lang.clone(),
        rate: "+0%".to_string(),
        rate_factor: cfg.tts.rate_factor,
        tempo: cfg.tts.tempo,
        target_rate: cfg.tts.target_rate,
        kbps: cfg.tts.kbps,
    }
}

/// Ruta on es desa el MP3 de la veu d'un link.
pub fn audio_path(cfg: &Config, link_id: Uuid) -> PathBuf {
    PathBuf::from(&cfg.tts.dir).join(format!("{link_id}.mp3"))
}

/// Cua global del TTS: Edge tanca la connexio si es fan diverses síntesis
/// seguides des de la mateixa IP (close 1006). Serialitzem les síntesis de tot
/// el procés i deixem `TTS_MIN_GAP_MS` entre l'inici de cada una.
fn tts_gate() -> &'static tokio::sync::Mutex<Option<Instant>> {
    static GATE: OnceLock<tokio::sync::Mutex<Option<Instant>>> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// Espera el torn dins la cua global del TTS. El guard manté la síntesi en
/// exclusió mútua fins que es deixa anar (després de l'intent).
async fn tts_turn(cfg: &Config) -> tokio::sync::MutexGuard<'static, Option<Instant>> {
    let mut last = tts_gate().lock().await;
    if let Some(prev) = *last {
        let gap = Duration::from_millis(cfg.tts.min_gap_ms);
        let elapsed = prev.elapsed();
        if elapsed < gap {
            tokio::time::sleep(gap - elapsed).await;
        }
    }
    *last = Some(Instant::now());
    last
}

/// Nombre de paraules (separades per blanc) d'un text.
pub fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

/// Cert si la paraula acaba una frase (`.`, `!`, `?`, `…`).
fn ends_sentence(word: &str) -> bool {
    let w = word.trim_end_matches(['"', '»', '”', ')', ']', '\'']);
    w.ends_with('.') || w.ends_with('!') || w.ends_with('?') || w.ends_with('…')
}

/// Cert si la paraula és una abreviatura que NO tanca frase («p. ex.», «etc.»).
/// Tallar-hi el resum deixaria el text penjat.
fn is_abbrev(word: &str) -> bool {
    let w = word.trim_end_matches(['.', ',', ';', ':']).to_lowercase();
    w.chars().count() == 1
        || matches!(
            w.as_str(),
            "etc" | "núm"
                | "num"
                | "art"
                | "cap"
                | "sr"
                | "sra"
                | "dr"
                | "dra"
                | "fig"
                | "pàg"
                | "pag"
                | "vol"
                | "ed"
                | "ex"
                | "aprox"
        )
}

/// Neteja un text per a la veu: treu markdown (via `clean_prose`), obertures
/// metalingüístiques («Resum:», «L'article descriu...»), cometes, salts de
/// línia i espais repetits. Retorna `None` si no queda res.
fn clean_for_speech(raw: &str) -> Option<String> {
    let prose = crate::overlay::clean_prose(raw);
    let prose = crate::pipeline::polish_summary(&prose);
    let mut out = String::with_capacity(prose.len());
    for c in prose.chars() {
        match c {
            '"' | '“' | '”' | '«' | '»' | '`' => {}
            '\n' | '\r' | '\t' => out.push(' '),
            _ => out.push(c),
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let out = out
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '—' | '–' | '·' | '•'))
        .trim();
    if out.is_empty() {
        None
    } else {
        Some(out.to_string())
    }
}

/// Tanca la frase si cal i treu puntuació penjant (comes/dos punts finals).
fn finish_sentence(s: &str) -> String {
    let mut out = s
        .trim_end_matches([',', ';', ':', '—', '–', '-'])
        .trim()
        .to_string();
    if out.is_empty() {
        return out;
    }
    if !ends_sentence(&out) {
        out.push('.');
    }
    out
}

/// Retalla un text a `max_words` paraules (frontera de paraula, amb punt final).
fn trim_to_words(text: &str, max_words: usize) -> Option<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let n = max_words.max(1).min(words.len());
    let out = finish_sentence(&words[..n].join(" "));
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Neteja i limita un text generat pel LLM perquè es pugui llegir en veu alta.
pub fn sanitize_script(raw: &str, max_words: usize) -> Option<String> {
    let clean = clean_for_speech(raw)?;
    if word_count(&clean) <= max_words {
        Some(finish_sentence(&clean))
    } else {
        trim_to_words(&clean, max_words)
    }
}

/// Extracció automàtica de reserva: les primeres frases de `text` fins a
/// ~`words` paraules (tall preferent en final de frase, sense abreviatures).
pub fn brief_from_text(text: &str, words: usize) -> Option<String> {
    let target = words.max(10);
    let clean = clean_for_speech(text)?;
    let raw: Vec<&str> = clean.split_whitespace().collect();
    if raw.is_empty() {
        return None;
    }
    let max = (target + 10).min(raw.len());
    let min = target.saturating_sub(10);
    // Última frontera de frase dins la finestra [min, max] (mai una abreviatura).
    let mut cut: Option<usize> = None;
    for (i, w) in raw.iter().enumerate().take(max) {
        let n = i + 1;
        if n >= min && ends_sentence(w) && !is_abbrev(w) {
            cut = Some(n);
        }
    }
    let n = cut.unwrap_or_else(|| target.min(raw.len()));
    let out = finish_sentence(&raw[..n].join(" "));
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Text que llegeix la veu: el titular i, si n'hi ha, el resum parlat.
pub fn spoken_text(title: Option<&str>, script: Option<&str>) -> String {
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    let script = script.map(str::trim).filter(|s| !s.is_empty());
    match (title, script) {
        (Some(t), Some(s)) if ends_sentence(t) => format!("{t} {s}"),
        (Some(t), Some(s)) => format!("{t}. {s}"),
        (Some(t), None) => t.to_string(),
        (None, Some(s)) => s.to_string(),
        (None, None) => String::new(),
    }
}

/// Genera el resum parlat d'una notícia: primer el LLM (fidel i cohesionat) i,
/// si falla o la resposta no té la llargada demanada, una extracció automàtica
/// de `deep_summary`. No retorna error: la veu és best-effort.
pub async fn script_for(
    cfg: &Config,
    llm: Option<&LlmClient>,
    title: Option<&str>,
    deep_summary: &str,
) -> Option<String> {
    let summary = deep_summary.trim();
    if summary.is_empty() {
        return None;
    }
    let target = cfg.tts.script_words;
    if let Some(client) = llm {
        // Amb el circuit obert (model caigut) no val la pena esperar la crida:
        // l'extracció de reserva és immediata i el text no es queda sense veu.
        if client.is_cooling_down() {
            tracing::debug!("resum parlat: LLM en cooldown; extracció de reserva");
            return brief_from_text(summary, target);
        }
        match client.brief(title, summary, target).await {
            Ok(raw) => match clean_for_speech(&raw) {
                Some(clean) => {
                    let n = word_count(&clean);
                    if (target.saturating_sub(SCRIPT_WORD_TOLERANCE)
                        ..=target + SCRIPT_WORD_TOLERANCE)
                        .contains(&n)
                    {
                        return sanitize_script(&clean, target + SCRIPT_WORD_TOLERANCE);
                    }
                    tracing::info!(
                        words = n,
                        target,
                        "resum parlat del LLM fora de marge; extracció de reserva"
                    );
                }
                None => tracing::warn!("resum parlat del LLM buit; extracció de reserva"),
            },
            Err(e) => {
                tracing::warn!(error = %e, "resum parlat: el LLM ha fallat; extracció de reserva")
            }
        }
    }
    brief_from_text(summary, target)
}

/// Genera (best-effort) el MP3 del text parlat d'un link.
///
/// Retorna `true` si la veu s'ha generat i desat correctament. Mai retorna
/// error: els problemes només s'escriuen amb `tracing::warn!`.
pub async fn maybe_generate(
    db: &Db,
    cfg: &Config,
    link_id: Uuid,
    text: &str,
    link_type: LinkType,
) -> bool {
    if !applies(cfg, link_type) {
        return false;
    }
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    // El servei admet ~4 KB, però per seguretat no enviem mai més de 2000
    // bytes de text (la veu el llegiria sencer; no hi ha retall previ).
    // Es retalla en frontera de caràcter per no partir un UTF-8.
    let text = if text.len() > 2000 {
        let mut end = 2000;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        tracing::warn!(%link_id, len = text.len(), "text de veu massa llarg; es retalla");
        &text[..end]
    } else {
        text
    };

    let vcfg = voice_config(cfg);
    let timeout = std::time::Duration::from_secs(cfg.tts.timeout_secs);
    // El servei d'Edge tanca connexions de tant en tant (close 1006 / reset):
    // reintentem un parell de cops abans de donar la notícia per sense veu.
    const SYNTH_ATTEMPTS: u32 = 3;
    let mut mp3: Option<Vec<u8>> = None;
    let mut last_err = String::new();
    for attempt in 1..=SYNTH_ATTEMPTS {
        // Torn global + espera mínima: evita les ràfegues que Edge talla.
        let turn = tts_turn(cfg).await;
        let res = tokio::time::timeout(timeout, clio_voice::synthesize_voice(text, &vcfg)).await;
        drop(turn);
        match res {
            Ok(Ok(bytes)) => {
                mp3 = Some(bytes);
                break;
            }
            Ok(Err(e)) => last_err = e.to_string(),
            Err(_) => last_err = format!("timeout de {} s", cfg.tts.timeout_secs),
        }
        if attempt < SYNTH_ATTEMPTS {
            tracing::warn!(%link_id, attempt, error = %last_err,
                           "veu de la notícia: reintent de sintesi");
            tokio::time::sleep(std::time::Duration::from_millis(1_500 * u64::from(attempt))).await;
        }
    }
    let Some(mp3) = mp3 else {
        tracing::warn!(%link_id, attempts = SYNTH_ATTEMPTS, error = %last_err,
                       "veu de la notícia: sintesi fallida");
        return false;
    };

    let path = audio_path(cfg, link_id);
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(%link_id, error = %e, "veu de la notícia: mkdir {dir:?} fallit");
            return false;
        }
    }
    if let Err(e) = std::fs::write(&path, &mp3) {
        tracing::warn!(%link_id, error = %e, "veu de la notícia: escriptura {path:?} fallida");
        return false;
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    match db.set_link_audio_file(link_id, Some(name)).await {
        Ok(()) => {
            tracing::info!(%link_id, bytes = mp3.len(), "veu de la notícia generada");
            true
        }
        Err(e) => {
            tracing::warn!(%link_id, error = %e, "veu de la notícia: db fallida");
            false
        }
    }
}

/// Prepara la veu d'una notícia en acabar l'anàlisi profunda: genera el text
/// parlat (titular + resum), el desa i en sintetitza el MP3. Sempre
/// best-effort: no propaga errors ni fa fallar el deep.
pub async fn prepare(
    db: &Db,
    cfg: &Config,
    llm: Option<&LlmClient>,
    link: &Link,
    deep_summary: &str,
) {
    if !applies(cfg, link.link_type) {
        return;
    }
    let Some(script) = script_for(cfg, llm, link.title.as_deref(), deep_summary).await else {
        tracing::debug!(link_id = %link.id, "sense text per a la veu");
        return;
    };
    // Es desa encara que la sintesi caigui: el backfill no ha de repetir el LLM.
    if let Err(e) = db.set_link_audio_script(link.id, Some(&script)).await {
        tracing::warn!(link_id = %link.id, error = %e, "no s'ha pogut desar el resum parlat");
    }
    let text = spoken_text(link.title.as_deref(), Some(&script));
    maybe_generate(db, cfg, link.id, &text, link.link_type).await;
}

/// Backfill de veu: genera o actualitza els MP3 de les notícies amb anàlisi
/// profunda que en manquen o que encara tenen la veu antiga (només titular).
/// Són les úniques que poden aparèixer a l'overlay. Reutilitza `audio_script`
/// si ja hi és; si no, el genera (LLM si està disponible) i el desa perquè un
/// reintent només hagi de sintetitzar.
/// Retorna (veus generades, links comprovats).
pub async fn backfill(
    db: &Db,
    cfg: &Config,
    llm: Option<&LlmClient>,
    limit: i64,
) -> Result<(usize, usize)> {
    let ids = db.deep_done_ids_pending_voice(limit).await?;
    let (mut generated, mut checked) = (0usize, 0usize);
    for id in ids {
        let Ok(Some(l)) = db.link_by_id(id).await else {
            continue;
        };
        if !applies(cfg, l.link_type) {
            continue; // no li toca veu
        }
        checked += 1;
        let script = match l
            .audio_script
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => s.to_string(),
            None => {
                let Some(deep) = l.deep_summary.as_deref() else {
                    continue;
                };
                // Amb el model en cooldown no desem una extracció de pitjor
                // qualitat de forma permanent: es reintentarà amb el LLM.
                if llm.map(|c| c.is_cooling_down()).unwrap_or(false) {
                    tracing::warn!(%id, "backfill: LLM en cooldown; es deixa per a un proper intent");
                    continue;
                }
                let Some(s) = script_for(cfg, llm, l.title.as_deref(), deep).await else {
                    continue;
                };
                if let Err(e) = db.set_link_audio_script(id, Some(&s)).await {
                    tracing::warn!(%id, error = %e, "backfill: no s'ha pogut desar el resum parlat");
                }
                s
            }
        };
        let text = spoken_text(l.title.as_deref(), Some(&script));
        if maybe_generate(db, cfg, id, &text, l.link_type).await {
            generated += 1;
        }
    }
    Ok((generated, checked))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text de prova amb frases de mida coneguda.
    fn words(n: usize, tag: &str) -> String {
        (0..n).map(|i| format!("{tag}{i}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn brief_prefers_sentence_boundary_near_target() {
        let text = format!("{}. {}. {}.", words(20, "a"), words(20, "b"), words(20, "c"));
        let out = brief_from_text(&text, 45).unwrap();
        let n = word_count(&out);
        assert!((35..=55).contains(&n), "llargada dins del marge: {n} ({out:?})");
        assert!(out.starts_with("a0 a1"), "comença pel principi: {out:?}");
        assert!(out.ends_with('.'), "acaba en punt: {out:?}");
        // 40 paraules = dues frases senceres (frontera preferida a 40, no a 45).
        assert_eq!(n, 40, "talla en final de frase: {out:?}");
    }

    #[test]
    fn brief_trims_a_very_long_first_sentence() {
        let text = words(100, "x");
        let out = brief_from_text(&text, 45).unwrap();
        assert_eq!(word_count(&out), 45, "retalla a l'objectiu: {out:?}");
        assert!(out.ends_with('.'), "tanca la frase encara que es talli: {out:?}");
    }

    #[test]
    fn brief_ignores_abbreviation_as_sentence_end() {
        // Frontera falsa a la paraula 36 («p.») i la frase real acaba a 60.
        let mut parts: Vec<String> = (0..35).map(|i| format!("w{i}")).collect();
        parts.push("p.".into());
        parts.push("ex.".into());
        parts.extend((0..23).map(|i| format!("z{i}")));
        let out = brief_from_text(&parts.join(" "), 45).unwrap();
        assert_eq!(word_count(&out), 45, "no talla a l'abreviatura: {out:?}");
    }

    #[test]
    fn brief_returns_none_without_text() {
        assert_eq!(brief_from_text("", 45), None);
        assert_eq!(brief_from_text("   \n\n ", 45), None);
        assert_eq!(brief_from_text("> cita\n- ", 45), None);
    }

    #[test]
    fn sanitize_strips_markdown_meta_and_quotes() {
        let raw = "## Resum: **El mercat** puja \"amb força\"\n- segona línia";
        let out = sanitize_script(raw, 45).unwrap();
        assert!(!out.contains('*') && !out.contains('#'), "sense markdown: {out:?}");
        assert!(!out.contains('"'), "sense cometes: {out:?}");
        assert!(!out.contains('\n'), "un sol paràgraf: {out:?}");
        assert!(out.starts_with("El mercat"), "treu el label «Resum»: {out:?}");
        assert!(out.ends_with('.'), "acaba en punt: {out:?}");
    }

    #[test]
    fn sanitize_caps_word_count() {
        let raw = words(80, "p");
        let out = sanitize_script(&raw, 45).unwrap();
        assert_eq!(word_count(&out), 45, "retalla al màxim: {out:?}");
        assert!(out.ends_with('.'), "tanca la frase: {out:?}");
    }

    #[test]
    fn spoken_text_joins_title_and_script() {
        assert_eq!(
            spoken_text(Some("El mercat puja"), Some("Ho fa per la demanda.")),
            "El mercat puja. Ho fa per la demanda."
        );
        // Si el titular ja tanca frase, no s'hi afegeix un segon punt.
        assert_eq!(
            spoken_text(Some("Què ha passat?"), Some("Una vaga.")),
            "Què ha passat? Una vaga."
        );
        assert_eq!(spoken_text(None, Some("Només resum.")), "Només resum.");
        assert_eq!(spoken_text(Some("Només titular"), None), "Només titular");
        assert_eq!(spoken_text(None, None), "");
    }

    /// Sense LLM, `script_for` fa l'extracció automàtica de l'anàlisi profunda
    /// (la reserva que evita que una caiguda del model deixi la notícia sense veu).
    #[tokio::test]
    async fn script_for_without_llm_extracts_from_deep_summary() {
        std::env::remove_var("TTS_SCRIPT_WORDS");
        let cfg = Config::from_env().unwrap();
        let deep = format!("{}. {}.", words(20, "a"), words(20, "b"));
        let s = script_for(&cfg, None, Some("Títol"), &deep)
            .await
            .expect("extracció de reserva");
        let n = word_count(&s);
        assert!((35..=55).contains(&n), "llargada de resum: {n} ({s:?})");
        assert!(s.ends_with('.'), "acaba en punt: {s:?}");
        // Sense text no hi ha res a dir.
        assert!(script_for(&cfg, None, Some("Títol"), "   ").await.is_none());
    }

    #[test]
    fn word_count_ignores_whitespace_runs() {
        assert_eq!(word_count("  un   dos\ttres\n"), 3);
        assert_eq!(word_count(""), 0);
    }
}
