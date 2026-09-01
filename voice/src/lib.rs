//! Clio Voice — TTS (protocol Read Aloud de Microsoft Edge, "msedge-tts")
//! + transformacio d'audio *per codi* en Rust: port de `voice-convert` (Node).
//!
//! La cadena es descompon en etapes DSP pures (cap dependence de ffmpeg):
//!
//!   ffmpeg -af "asetrate=24000*0.75,aresample=24000,atempo=2"
//!   = resampleLinear(1/0.75)  -> to x0.75 (i durada x1.333)
//!   + timeScale(2)            -> phase vocoder (velocitat x2 conservant el to)
//!   => net: veu mes greu (x0.75) i mes rapida (x1.5 / durada 2/3).

pub mod audio;
pub mod edge;

/// Errors del pipeline de veu.
#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("edge tts: {0}")]
    Edge(String),
    #[error("audio decode: {0}")]
    Decode(String),
    #[error("audio encode: {0}")]
    Encode(String),
}

pub const DEFAULT_VOICE: &str = "ca-ES-JoanaNeural";
pub const DEFAULT_LANG: &str = "ca-ES";
pub const DEFAULT_RATE: &str = "+0%";
pub const DEFAULT_RATE_FACTOR: f64 = 0.75;
pub const DEFAULT_TEMPO: f64 = 2.0;
pub const DEFAULT_TARGET_RATE: u32 = 24000;
pub const DEFAULT_KBPS: u32 = 48;

/// Configuracio de la veu + transform (mirall de voice-convert).
#[derive(Debug, Clone)]
pub struct VoiceConfig {
    pub voice: String,
    pub lang: String,
    /// prosody `rate` de l'SSML ("+0%" per defecte; el canvi real el fa el DSP).
    pub rate: String,
    /// to: `asetrate<rate_factor>`.
    pub rate_factor: f64,
    /// velocitat: `atempo<tempo>`.
    pub tempo: f64,
    /// sample rate de sortida.
    pub target_rate: u32,
    /// bitrate del MP3 final.
    pub kbps: u32,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            voice: DEFAULT_VOICE.to_string(),
            lang: DEFAULT_LANG.to_string(),
            rate: DEFAULT_RATE.to_string(),
            rate_factor: DEFAULT_RATE_FACTOR,
            tempo: DEFAULT_TEMPO,
            target_rate: DEFAULT_TARGET_RATE,
            kbps: DEFAULT_KBPS,
        }
    }
}

impl VoiceConfig {
    pub fn transform_options(&self) -> audio::transform::TransformOptions {
        audio::transform::TransformOptions {
            rate_factor: self.rate_factor,
            tempo: self.tempo,
            target_rate: self.target_rate,
        }
    }
}

/// Sintetitza el text amb la veu d'Edge i torna el MP3 cru (24 kHz mono).
pub async fn synthesize_text(text: &str, cfg: &VoiceConfig) -> Result<Vec<u8>, VoiceError> {
    // Els titulars son curts (~80 car.); el servei admet fins a ~4 KB.
    edge::synthesize(text, &cfg.voice, &cfg.lang, &cfg.rate).await
}

/// Transforma un MP3 (decode -> asetrate+atempo per codi -> encode).
pub fn transform_mp3(mp3: &[u8], cfg: &VoiceConfig) -> Result<Vec<u8>, VoiceError> {
    audio::transform::transform_mp3_to_mp3(mp3, &cfg.transform_options(), cfg.kbps)
}

/// Operacio completa que usara Clio: sintetitza el titol i el transforma,
/// enllestint el MP3 final (24 kHz mono, veu greu i rapida).
pub async fn synthesize_voice(text: &str, cfg: &VoiceConfig) -> Result<Vec<u8>, VoiceError> {
    let raw = synthesize_text(text, cfg).await?;
    transform_mp3(&raw, cfg)
}
