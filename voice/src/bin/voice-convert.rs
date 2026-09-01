//! CLI de `voice-convert` (port Rust). Paritat amb el projecte Node:
//!
//!   voice-convert "Hola, aixo es una prova"
//!   voice-convert --input el-meu.mp3
//!   voice-convert --voice ca-ES-AlbaNeural --rate-factor 0.75 --tempo 2 ...
//!   voice-convert --selftest        # DSP offline (com `npm run selftest`)

use clap::Parser;
use clio_voice::audio::{decode, dsp, transform, wav};
use clio_voice::audio::analyze::estimate_dominant_frequency_default as estimate_freq;
use clio_voice::{VoiceConfig, DEFAULT_VOICE};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "voice-convert",
    about = "TTS (msedge-tts) + transformacio d'audio per codi en Rust (port de voice-convert)",
    version
)]
struct Cli {
    /// Text a sintetitzar (si no s'usa --input)
    text: Option<String>,
    /// Veu Edge TTS (per defecte ca-ES-JoanaNeural)
    #[arg(long, default_value = DEFAULT_VOICE)]
    voice: String,
    /// Usa un MP3 existent en lloc de fer TTS
    #[arg(long)]
    input: Option<PathBuf>,
    /// asetrate: factor de to (0.75)
    #[arg(long, default_value_t = 0.75)]
    rate_factor: f64,
    /// atempo: factor de velocitat (2.0)
    #[arg(long, default_value_t = 2.0)]
    tempo: f64,
    /// -ar sample rate de sortida (24000)
    #[arg(long, default_value_t = 24000u32)]
    target_rate: u32,
    /// Bitrate MP3 de sortida (48)
    #[arg(long, default_value_t = 48u32)]
    kbps: u32,
    /// Ruta de sortida (out/result.mp3)
    #[arg(long, default_value = "out/result.mp3")]
    out: PathBuf,
    /// Tambe escriu el resultat en WAV (out/result.wav)
    #[arg(long)]
    wav: bool,
    /// Self-test offline de la DSP (sense xarxa)
    #[arg(long)]
    selftest: bool,
}

fn cfg_from(cli: &Cli) -> VoiceConfig {
    VoiceConfig {
        voice: cli.voice.clone(),
        rate_factor: cli.rate_factor,
        tempo: cli.tempo,
        target_rate: cli.target_rate,
        kbps: cli.kbps,
        ..Default::default()
    }
}

fn approx(actual: f64, expected: f64, tolerance: f64) -> bool {
    (actual - expected).abs() <= expected.abs() * tolerance
}

/// Mirall del `npm run selftest` de voice-convert: 100% offline.
fn run_selftest() -> Result<(), String> {
    let rate = 24000u32;
    let mut failures = Vec::new();
    let mut check = |ok: bool, label: String| {
        println!("  {} - {label}", if ok { "OK " } else { "FAIL" });
        if !ok {
            failures.push(label);
        }
    };

    // atempo pur (phase vocoder): pitch es conserva.
    let n = rate as usize;
    let tone: Vec<f64> = (0..n)
        .map(|i| 0.5 * (2.0 * std::f64::consts::PI * 300.0 * i as f64 / rate as f64).sin())
        .collect();
    let scaled = dsp::time_scale(&tone, 2.0);
    let scaled_dur = scaled.len() as f64 / rate as f64;
    let scaled_freq = estimate_freq(&scaled, rate);
    check(
        approx(scaled_dur, 0.5, 0.06),
        format!("atempo: durada ~0.5s (real {scaled_dur:.3}s)"),
    );
    check(
        approx(scaled_freq, 300.0, 0.03),
        format!("atempo: frequencia ~300 Hz (real {scaled_freq:.1} Hz)"),
    );

    // cadena completa (equivalent ffmpeg).
    let tone_pcm = clio_voice::audio::PcmAudio { samples: tone.iter().map(|&v| v as f32).collect(), sample_rate: rate };
    let transformed = transform::transform_pcm(&tone_pcm, &transform::TransformOptions::default());
    let out_dur = transformed.duration_seconds();
    let out_freq_v: Vec<f64> = transformed.samples.iter().map(|&x| x as f64).collect();
    let out_freq = estimate_freq(&out_freq_v, transformed.sample_rate);
    check(
        approx(out_dur, 2.0 / 3.0, 0.06),
        format!("cadena: durada ~0.667s (real {out_dur:.3}s)"),
    );
    check(
        approx(out_freq, 225.0, 0.02),
        format!("cadena: frequencia ~225 Hz (real {out_freq:.1} Hz)"),
    );

    // ronda MP3 (encode + decode).
    match clio_voice::audio::encode::encode_mp3(&transformed, 48) {
        Ok(mp3) => {
            check(mp3.len() > 1000, format!("mp3: generat ({} bytes)", mp3.len()));
            match decode::decode_mp3(&mp3) {
                Ok(decoded) => {
                    let dec_dur = decoded.duration_seconds();
                    let dec_freq_v: Vec<f64> = decoded.samples.iter().map(|&x| x as f64).collect();
                    let dec_freq = estimate_freq(&dec_freq_v, decoded.sample_rate);
                    let dec_peak = decoded.peak_abs();
                    check(decoded.sample_rate > 0, format!("decode: amb {} Hz", decoded.sample_rate));
                    check(dec_peak > 0.1, format!("decode: NO es silenci (peak {dec_peak:.3})"));
                    check(
                        approx(dec_dur, 0.667, 0.20),
                        format!("decode: durada ~0.667s (real {dec_dur:.3}s)"),
                    );
                    check(
                        approx(dec_freq, 225.0, 0.05),
                        format!("decode: freq ~225 Hz (real {dec_freq:.1} Hz)"),
                    );
                }
                Err(e) => check(false, format!("decode: fallat ({e})")),
            }
        }
        Err(e) => check(false, format!("mp3: encode fallat ({e})")),
    }

    if failures.is_empty() {
        println!("TOTS ELS TESTS HAN PASSAT");
        Ok(())
    } else {
        Err(format!("self-test fallat: {} checks", failures.len()))
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if cli.selftest {
        match run_selftest() {
            Ok(()) => {}
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let res: Result<Vec<u8>, String> = async {
        let cfg = cfg_from(&cli);
        if let Some(input) = &cli.input {
            let bytes = std::fs::read(input)
                .map_err(|e| format!("no s'ha pogut llegir {}: {e}", input.display()))?;
            clio_voice::transform_mp3(&bytes, &cfg).map_err(|e| e.to_string())
        } else {
            let text = cli
                .text
                .clone()
                .ok_or_else(|| "cal passar un text o --input".to_string())?;
            clio_voice::synthesize_voice(&text, &cfg)
                .await
                .map_err(|e| e.to_string())
        }
    }
    .await;

    match res {
        Ok(bytes) => {
            if let Some(parent) = cli.out.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&cli.out, &bytes) {
                eprintln!("no s'ha pogut escriure {}: {e}", cli.out.display());
                std::process::exit(1);
            }
            println!(
                "MP3 escrit: {} ({} kB)",
                cli.out.display(),
                bytes.len() / 1024
            );
            if cli.wav {
                let decoded = decode::decode_mp3(&bytes)
                    .map_err(|e| format!("decode per a wav: {e}"))
                    .expect("decode");
                let wav_path = cli.out.with_extension("wav");
                let mut wav_bytes = Vec::new();
                wav::write_wav16(&decoded, &mut wav_bytes)
                    .expect("wav");
                std::fs::write(&wav_path, &wav_bytes).expect("write wav");
                println!("WAV escrit: {}", wav_path.display());
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
