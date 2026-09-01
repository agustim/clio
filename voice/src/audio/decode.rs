//! Decodificacio de MP3 a PCM f32 amb `symphonia` (pur Rust, sense ffmpeg).

use super::{err_decode, PcmAudio};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Decodifica un MP3 (qualsevol capa III de MPEG-1/2/2.5) a PCM mono f32.
/// Fa downmix a mono si el fitxer te mes d'un canal.
pub fn decode_mp3(data: &[u8]) -> Result<PcmAudio, crate::VoiceError> {
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| err_decode(format!("probe: {e}")))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| err_decode("cap pista d'audio al fitxer"))?;
    let track_id = track.id;

    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|cp| cp.audio())
        .ok_or_else(|| err_decode("la pista no es d'audio"))?;

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
        .map_err(|e| err_decode(format!("codec: {e}")))?;

    let mut interleaved: Vec<f32> = Vec::new();
    let mut sample_rate = codec_params.sample_rate.unwrap_or(24000);
    let mut channels = codec_params
        .channels
        .as_ref()
        .map(|c| c.count())
        .unwrap_or(1);

    loop {
        match format.next_packet() {
            Ok(Some(packet)) => {
                if packet.track_id != track_id {
                    continue;
                }
                match decoder.decode(&packet) {
                    Ok(decoded) => {
                        sample_rate = decoded.spec().rate();
                        let ch = decoded.spec().channels().count();
                        channels = ch;
                        let mut dst = vec![0.0f32; decoded.samples_interleaved()];
                        decoded.copy_to_slice_interleaved(&mut dst);
                        interleaved.extend_from_slice(&dst);
                    }
                    // Frames corruptes (rars en MP3 CBR): saltem i seguim.
                    Err(SymError::DecodeError(_)) => continue,
                    Err(e) => return Err(err_decode(format!("decode: {e}"))),
                }
            }
            // EOF
            Ok(None) => break,
            // Errors de format a mig flux: per a un MP3 CBR petit, ho acabem.
            Err(SymError::DecodeError(_)) => break,
            Err(SymError::IoError(_)) => break,
            Err(e) => return Err(err_decode(format!("format: {e}"))),
        }
    }

    if interleaved.is_empty() {
        return Err(err_decode("MP3 sense mostres decodificades"));
    }

    let samples = if channels > 1 {
        downmix_to_mono(&interleaved, channels)
    } else {
        interleaved
    };

    Ok(PcmAudio { samples, sample_rate })
}

/// Mitjana de canals intercalats -> un sol canal.
fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    let frames = interleaved.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut sum = 0.0f32;
        for c in 0..channels {
            sum += interleaved[f * channels + c];
        }
        mono.push(sum / channels as f32);
    }
    mono
}
