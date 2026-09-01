//! Audio: tipus PCM compartits, decodificacio MP3 (symphonia), transformacio
//! DSP per codi (port de `voice-convert`) i codificacio MP3 (shine-rs).

pub mod analyze;
pub mod decode;
pub mod dsp;
pub mod encode;
pub mod transform;
pub mod wav;

use crate::VoiceError;

/// PCM monofonic en f32, com el `PcmAudio` de voice-convert (pero mono, que es
/// el que fa servir tot el pipeline: Edge surt a 24 kHz mono i aqui sempre
/// treballem amb un sol canal).
#[derive(Debug, Clone)]
pub struct PcmAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl PcmAudio {
    pub fn duration_seconds(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.samples.len() as f64 / self.sample_rate as f64
        }
    }

    pub fn peak_abs(&self) -> f32 {
        self.samples.iter().fold(0.0f32, |m, &v| m.max(v.abs()))
    }
}

/// Torna un error concret de l'etapa d'audio.
pub(crate) fn err_decode(msg: impl std::fmt::Display) -> VoiceError {
    VoiceError::Decode(msg.to_string())
}
