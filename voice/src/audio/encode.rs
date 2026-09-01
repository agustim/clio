//! Codificacio de PCM a MP3 amb `shine-rs` (encoder MP3 pur en Rust).

use super::PcmAudio;
use crate::VoiceError;
use shine_rs::{Mp3EncoderConfig, StereoMode};

const INT16_MAX: f64 = 32767.0;

fn pcm_to_int16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&v| {
            let c = (v.clamp(-1.0, 1.0) as f64) * INT16_MAX;
            c.round() as i16
        })
        .collect()
}

/// Codifica un PCM (mono o estereo) a MP3 a `kbps`.
pub fn encode_mp3(pcm: &PcmAudio, kbps: u32) -> Result<Vec<u8>, VoiceError> {
    let channels = 1u8; // sempre mono: les veus d'Edge surten mono i aixi les barrejara ffmpeg
    let stereo_mode = StereoMode::Mono;
    let config = Mp3EncoderConfig::new()
        .sample_rate(pcm.sample_rate)
        .bitrate(kbps)
        .channels(channels)
        .stereo_mode(stereo_mode)
        .original(true);

    let int16 = pcm_to_int16(&pcm.samples);
    shine_rs::encode_pcm_to_mp3(config, &int16)
        .map_err(|e| VoiceError::Encode(format!("shine: {e}")))
}
