//! Cadena de transformacio equivalent a la comanda ffmpeg del voice-convert:
//!
//!   ffmpeg -i input.mp3 -af "asetrate=24000*R,aresample=24000,atempo=T" ...
//!
//! Implementada per codi:
//!   1. `resample_linear(x, 1/R)`  -> re-mostreig que allarga (x1/R) i baixa el to (R).
//!   2. `time_scale(x, T)`         -> phase vocoder que dobla... canvia la velocitat
//!                                    conservant el to (atempo).
//!
//! Resultat net: durada = 2/3 (velocitat x1.5) i to = 0.75 amb els defaults.

use super::dsp;
use super::{decode, encode, PcmAudio};
use crate::VoiceError;

/// Parametres de la transformacio (equivalents a `FFMPEG_DEFAULTS`).
#[derive(Debug, Clone, Copy)]
pub struct TransformOptions {
    /// to: factor multiplicador del pitch (el "asetrate*R").
    pub rate_factor: f64,
    /// velocitat: factor `atempo` (canvia la durada conservant el to).
    pub tempo: f64,
    /// sample rate de sortida (aquest sempre es el del PCM d'entrada a la
    /// sortida; s'usa per declarar-ho).
    pub target_rate: u32,
}

impl Default for TransformOptions {
    fn default() -> Self {
        Self {
            rate_factor: 0.75,
            tempo: 2.0,
            target_rate: 24000,
        }
    }
}

/// Transforma un canal (f64) amb resample + phase vocoder.
fn transform_channel(x: &[f64], opts: &TransformOptions) -> Vec<f64> {
    let slowed = dsp::resample_linear(x, 1.0 / opts.rate_factor);
    dsp::time_scale(&slowed, opts.tempo)
}

/// Aplica la cadena completa a un PCM (mono o multi-canal).
pub fn transform_pcm(input: &PcmAudio, opts: &TransformOptions) -> PcmAudio {
    let channel: Vec<f64> = input.samples.iter().map(|&v| v as f64).collect();
    let finished = transform_channel(&channel, opts);
    PcmAudio {
        samples: finished.into_iter().map(|v| v as f32).collect(),
        sample_rate: opts.target_rate,
    }
}

/// Transforma un MP3 directament: decode -> transform -> encode.
/// Es la operacio que usara Clio per generar el MP3 final dels titulars.
pub fn transform_mp3_to_mp3(
    mp3_in: &[u8],
    opts: &TransformOptions,
    kbps: u32,
) -> Result<Vec<u8>, VoiceError> {
    let decoded = decode::decode_mp3(mp3_in)?;
    let transformed = transform_pcm(&decoded, opts);
    let out = encode::encode_mp3(&transformed, kbps)?;
    if out.is_empty() {
        return Err(VoiceError::Encode("encoder no ha produït res".into()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::analyze::estimate_dominant_frequency;
    use crate::audio::encode::encode_mp3;

    fn make_tone(freq: f64, seconds: f64, rate: u32) -> PcmAudio {
        let n = (seconds * rate as f64).round() as usize;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                0.5 * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32
            })
            .collect();
        PcmAudio { samples, sample_rate: rate }
    }

    /// La cadena completa sobre un to de 300 Hz dona ~225 Hz i ~0.667 s
    /// (exactament el selftest de voice-convert).
    #[test]
    fn full_chain_300hz_becomes_225hz_and_2of3_duration() {
        let tone = make_tone(300.0, 1.0, 24000);
        let out = transform_pcm(&tone, &TransformOptions::default());
        let dur = out.duration_seconds();
        assert!(
            (dur - 2.0 / 3.0).abs() <= (2.0 / 3.0) * 0.06,
            "durada ~0.667s, era {dur}"
        );
        let freq_v: Vec<f64> = out.samples.iter().map(|&x| x as f64).collect();
        let freq = estimate_dominant_frequency(&freq_v, out.sample_rate, 50.0, 4000.0);
        assert!(
            (freq - 225.0).abs() <= 225.0 * 0.02,
            "freq ~225 Hz, era {freq}"
        );
    }

    /// Roundtrip MP3: transformem el to, el codifiquem, el decodifiquem i
    /// comprovem que NO es silenci, durada ~0.667 s i freq ~225 Hz (l'encoder
    /// aporta un xic de delay/padding, per aixo tolerancies mes amples).
    #[test]
    fn mp3_roundtrip_transformed_tone() {
        let tone = make_tone(300.0, 1.0, 24000);
        let out = transform_pcm(&tone, &TransformOptions::default());
        let mp3 = encode_mp3(&out, 48).expect("encode");
        assert!(mp3.len() > 400, "MP3 massa petit: {} bytes", mp3.len());

        let decoded = decode::decode_mp3(&mp3).expect("decode");
        assert_eq!(decoded.sample_rate, 24000);
        let dur = decoded.duration_seconds();
        assert!(
            (dur - 0.667).abs() <= 0.667 * 0.20,
            "durada post-mp3 ~0.667s (enc. delay), era {dur}"
        );
        let dec_v: Vec<f64> = decoded.samples.iter().map(|&x| x as f64).collect();
        let freq = estimate_dominant_frequency(&dec_v, decoded.sample_rate, 50.0, 4000.0);
        assert!(
            (freq - 225.0).abs() <= 225.0 * 0.05,
            "freq post-mp3 ~225 Hz, era {freq}"
        );
        assert!(decoded.peak_abs() > 0.03, "MP3 decodificat massa silencios");
    }
}
