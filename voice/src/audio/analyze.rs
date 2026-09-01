//! Estimacio de la frequencia dominant (per al selftest / tests de DSP).
//! Port de `src/audio/analyze.ts` de voice-convert.

/// GOERTZEL-like per força bruta sobre una graella de 800 passos entre
/// `min_hz` i `max_hz`: retorna la freqüencia amb mes energia.
pub fn estimate_dominant_frequency(samples: &[f64], sample_rate: u32, min_hz: f64, max_hz: f64) -> f64 {
    let n = samples.len();
    let margin = (n as f64 * 0.15).floor() as usize;
    let seg_start = margin;
    let seg_end = n.saturating_sub(margin);
    if seg_end <= seg_start {
        return 0.0;
    }

    const STEPS: usize = 800;
    let mut best_freq = 0.0f64;
    let mut best_energy = -1.0f64;
    for i in 0..=STEPS {
        let f = min_hz + ((max_hz - min_hz) * i as f64) / STEPS as f64;
        let w = 2.0 * std::f64::consts::PI * f / sample_rate as f64;
        let mut re = 0.0f64;
        let mut im = 0.0f64;
        for (k, &s) in samples.iter().enumerate().take(seg_end).skip(seg_start) {
            re += s as f64 * (w * k as f64).cos();
            im -= s as f64 * (w * k as f64).sin();
        }
        let energy = re * re + im * im;
        if energy > best_energy {
            best_energy = energy;
            best_freq = f;
        }
    }
    best_freq
}

/// Conveniencia amb els rang per defecte (50–4000 Hz), com el selftest.
pub fn estimate_dominant_frequency_default(samples: &[f64], sample_rate: u32) -> f64 {
    estimate_dominant_frequency(samples, sample_rate, 50.0, 4000.0)
}
