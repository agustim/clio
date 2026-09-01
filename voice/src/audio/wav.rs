//! Escriptor WAV PCM 16 bits mono (per a `--wav` / depuracio), sense deps.

use crate::VoiceError;

/// Escriu un WAV PCM 16-bit (mono) a partir del PCM f32.
pub fn write_wav16(pcm: &super::PcmAudio, out: &mut Vec<u8>) -> Result<(), VoiceError> {
    let sample_rate = pcm.sample_rate;
    let samples: Vec<i16> = pcm
        .samples
        .iter()
        .map(|&v| {
            let c = (v.clamp(-1.0, 1.0) as f64) * 32767.0;
            c.round() as i16
        })
        .collect();

    let data_len = (samples.len() * 2) as u32;
    let chunk_size = 36 + data_len;

    out.clear();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&chunk_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    Ok(())
}
