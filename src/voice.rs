//! Veu de titulars (TTS Edge + transform en Rust via `clio-voice`).
//!
//! En analitzar la cua (pipeline), per cada titular es genera un MP3 a
//! `TTS_DIR/{link_id}.mp3` i es desa el nom a `links.audio_file`. L'overlay
//! del directe el serveix a `/audio/{id}` i el llegeix dins del grup de cards.
//!
//! Tot es *best-effort*: mai fa fallar l'anàlisi d'un link (si la sintesi cau,
//! el link queda igualment `done` i només s'escriu un warning).

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;
use crate::models::LinkType;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

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

/// Genera (best-effort) el MP3 del titular d'un link.
///
/// Retorna `true` si la veu s'ha generat i desat correctament. Nunca retorna
/// error: els problemes només s'escriuen amb `tracing::warn!`.
pub async fn maybe_generate(
    db: &Db,
    cfg: &Config,
    link_id: Uuid,
    title: Option<&str>,
    link_type: LinkType,
) -> bool {
    if !applies(cfg, link_type) {
        return false;
    }
    let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) else {
        return false;
    };
    // El servei admet ~4 KB; els titols del pipeline van clampats a ~80 car.
    let title = if title.len() > 2000 {
        tracing::warn!(%link_id, len = title.len(), "titol massa llarg per a veu; es retalla");
        &title[..2000]
    } else {
        title
    };

    let vcfg = voice_config(cfg);
    let timeout = std::time::Duration::from_secs(cfg.tts.timeout_secs);
    let res = tokio::time::timeout(timeout, clio_voice::synthesize_voice(title, &vcfg)).await;
    let mp3 = match res {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            tracing::warn!(%link_id, error = %e, "veu del titular: sintesi fallida");
            return false;
        }
        Err(_) => {
            tracing::warn!(%link_id, timeout = cfg.tts.timeout_secs, "veu del titular: timeout");
            return false;
        }
    };

    let path = audio_path(cfg, link_id);
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(%link_id, error = %e, "veu del titular: mkdir {dir:?} fallit");
            return false;
        }
    }
    if let Err(e) = std::fs::write(&path, &mp3) {
        tracing::warn!(%link_id, error = %e, "veu del titular: escriptura {path:?} fallida");
        return false;
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    match db.set_link_audio_file(link_id, Some(name)).await {
        Ok(()) => {
            tracing::info!(%link_id, bytes = mp3.len(), "veu del titular generada");
            true
        }
        Err(e) => {
            tracing::warn!(%link_id, error = %e, "veu del titular: db fallida");
            false
        }
    }
}

/// Backfill de veu: genera els MP3 dels links processats que en manquen
/// (tipus aplicables i amb titol). Retorna (veu generades, links comprovats).
pub async fn backfill(db: &Db, cfg: &Arc<Config>, limit: i64) -> Result<(usize, usize)> {
    let ids = db.all_link_ids(limit).await?;
    let (mut generated, mut checked) = (0usize, 0usize);
    for id in ids {
        let Ok(Some(l)) = db.link_by_id(id).await else {
            continue;
        };
        if l.audio_file.is_some() || !applies(cfg, l.link_type) {
            continue; // ja te veu, o no li toca
        }
        let Some(title) = l.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        checked += 1;
        if maybe_generate(db, cfg, id, Some(title), l.link_type).await {
            generated += 1;
        }
    }
    Ok((generated, checked))
}
