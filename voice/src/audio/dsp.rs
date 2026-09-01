//! DSP per codi: re-mostreig lineal i phase vocoder (time-stretch amb
//! conservacio del to). Port fidel de `src/audio/{phaseVocoder,fft,resample}.ts`
//! de voice-convert, pero amb la FFT de `rustfft`.

use num_complex::Complex;

/// Finestra de Hann simetrica: `0.5*(1-cos(2π i / (n-1)))`.
pub fn hann_window(n: usize) -> Vec<f64> {
    let pi2 = std::f64::consts::PI * 2.0;
    let den = (n.saturating_sub(1) as f64).max(1.0);
    (0..n)
        .map(|i| 0.5 * (1.0 - (pi2 * i as f64 / den).cos()))
        .collect()
}

/// Envolta la fase a `(-π, π]`.
pub fn princarg(mut p: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let pi2 = pi * 2.0;
    while p > pi {
        p -= pi2;
    }
    while p <= -pi {
        p += pi2;
    }
    p
}

/// Interpolacio lineal en `pos` (fraccional). Clamped als extrems.
pub fn interpolate_at(x: &[f64], pos: f64) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let i = pos.floor() as isize;
    let frac = pos - i as f64;
    if i < 0 {
        return x[0];
    }
    if i as usize + 1 >= x.len() {
        return x[x.len() - 1];
    }
    let iu = i as usize;
    x[iu] * (1.0 - frac) + x[iu + 1] * frac
}

/// Re-mostreig per interpolacio lineal: `ratio > 1` allarga (i baixa el to),
/// `ratio < 1` escurça. Equivalent a `asetrate`/`aresample` de ffmpeg.
pub fn resample_linear(x: &[f64], ratio: f64) -> Vec<f64> {
    if x.is_empty() || ratio <= 0.0 || !ratio.is_finite() {
        return Vec::new();
    }
    let out_len = (x.len() as f64 * ratio).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        out.push(interpolate_at(x, i as f64 / ratio));
    }
    out
}

/// Phase vocoder: canvia la durada (tempo) conservant el to.
/// `tempo_factor > 1` => mes rapid (durada /tempo). Port de `timeScale` de
/// voice-convert (finestra Hann 1024, hop d'analisi 256, propagacio de fase
/// per bin i solapament-ponderat amb `.pot`).
pub fn time_scale(input: &[f64], tempo_factor: f64) -> Vec<f64> {
    time_scale_opts(input, tempo_factor, 1024, 256)
}

pub fn time_scale_opts(input: &[f64], tempo_factor: f64, n_fft: usize, analysis_hop: usize) -> Vec<f64> {
    if tempo_factor <= 0.0 || !tempo_factor.is_finite() || input.is_empty() {
        return Vec::new();
    }
    let pi2 = std::f64::consts::PI * 2.0;
    let stretch = 1.0 / tempo_factor;
    let synthesis_hop = analysis_hop as f64 * stretch;
    let nbins = n_fft / 2 + 1;
    let window = hann_window(n_fft);
    // `frames`: quants trams de n_fft caben (mateixa formula que el TS).
    let frames = ((input.len().saturating_sub(n_fft)) / analysis_hop)
        .saturating_add(1)
        .max(1);

    let out_len = (((frames - 1) as f64) * synthesis_hop).ceil() as usize + n_fft + 16;
    let mut out = vec![0.0f64; out_len];
    let mut norm = vec![0.0f64; out_len];

    let mut planner = rustfft::FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n_fft);
    let inv = planner.plan_fft_inverse(n_fft);

    let mut buffer: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); n_fft];
    let mut mag = vec![0.0f64; nbins];
    let mut phase = vec![0.0f64; nbins];
    let mut prev_phase = vec![0.0f64; nbins];
    let mut synth_phase = vec![0.0f64; nbins];
    let mut frame_buf: Vec<Complex<f64>> = vec![Complex::new(0.0, 0.0); n_fft];

    let mut started = false;
    let mut last_end = 0usize;

    for m in 0..frames {
        let start = m * analysis_hop;
        for j in 0..n_fft {
            let s = start + j;
            let v = if s < input.len() { input[s] } else { 0.0 };
            buffer[j] = Complex::new(v * window[j], 0.0);
        }
        fwd.process(&mut buffer);

        for k in 0..nbins {
            mag[k] = buffer[k].norm();
            phase[k] = buffer[k].arg();
        }

        if !started {
            synth_phase.copy_from_slice(&phase);
            started = true;
        } else {
            for k in 0..nbins {
                let expected_ha = pi2 * k as f64 * analysis_hop as f64 / n_fft as f64;
                let delta = princarg(phase[k] - prev_phase[k] - expected_ha);
                let expected_hs = pi2 * k as f64 * synthesis_hop / n_fft as f64;
                synth_phase[k] = synth_phase[k] + expected_hs + stretch * delta;
            }
        }
        prev_phase.copy_from_slice(&phase);

        // Reconstruim un espectre simetric conjugat i fem la inversa.
        for k in 0..nbins {
            frame_buf[k] = Complex::new(
                mag[k] * synth_phase[k].cos(),
                mag[k] * synth_phase[k].sin(),
            );
            let mirror = if k == 0 || k == n_fft / 2 { k } else { n_fft - k };
            frame_buf[mirror] = Complex::new(frame_buf[k].re, -frame_buf[k].im);
        }
        inv.process(&mut frame_buf);

        let out_start = (m as f64 * synthesis_hop).round() as usize;
        let mut end = 0usize;
        for j in 0..n_fft {
            let o = out_start + j;
            if o < out_len {
                out[o] += frame_buf[j].re * window[j];
                norm[o] += window[j] * window[j];
                if o + 1 > end {
                    end = o + 1;
                }
            }
        }
        if end > last_end {
            last_end = end;
        }
    }

    // La FFT inversa de rustfft no normalitza; el port divideix per n.
    let inv_scale = 1.0 / n_fft as f64;
    let mut result = vec![0.0f64; last_end];
    for i in 0..last_end {
        result[i] = if norm[i] > 1e-8 {
            out[i] / norm[i] * inv_scale
        } else {
            0.0
        };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resample_linear(ratio)` canvia la durada amb la ratio demanada.
    #[test]
    fn resample_ratio_changes_length() {
        let x: Vec<f64> = (0..2400).map(|i| (i as f64) * 0.001).collect();
        let longer = resample_linear(&x, 1.0 / 0.75);
        let shorter = resample_linear(&x, 0.5);
        let exp_long = (2400.0f64 / 0.75).round().max(1.0) as usize;
        assert_eq!(longer.len(), exp_long);
        assert_eq!(shorter.len(), (2400.0f64 * 0.5).round() as usize);
        assert!(resample_linear(&[], 2.0).is_empty());
    }

    /// El phase vocoder conserva el to i dobla la velocitat (durada ~0.5s).
    #[test]
    fn time_scale_keeps_pitch_changes_duration() {
        let rate = 24000.0;
        let n = rate as usize;
        let tone: Vec<f64> = (0..n)
            .map(|i| 0.5 * (2.0 * std::f64::consts::PI * 300.0 * i as f64 / rate).sin())
            .collect();
        let scaled = time_scale(&tone, 2.0);
        let dur = scaled.len() as f64 / rate;
        assert!((dur - 0.5).abs() <= 0.5 * 0.06, "durada ~0.5s, era {dur}");
        let freq = crate::audio::analyze::estimate_dominant_frequency(&scaled, 24000, 50.0, 4000.0);
        assert!(
            (freq - 300.0).abs() <= 300.0 * 0.03,
            "freq ~300 Hz, era {freq}"
        );
    }

    /// Hann simetrica: extrems 0, pic ~1 i simetria exacta.
    #[test]
    fn hann_symmetric() {
        let w = hann_window(1024);
        assert!((w[0] - 0.0).abs() < 1e-9);
        assert!(w[512] > 0.9999, "pic ~1, era {}", w[512]);
        assert!((w[1023] - 0.0).abs() < 1e-9);
        for i in 0..512 {
            assert!((w[i] - w[1023 - i]).abs() < 1e-12, "simetria a {i}");
        }
    }

    /// `princarg` sempre queda dins (-π, π].
    #[test]
    fn princarg_wraps() {
        assert!((princarg(7.0) - (7.0 - std::f64::consts::PI * 2.0)).abs() < 1e-12);
        assert!((princarg(-7.0) - (-7.0 + std::f64::consts::PI * 2.0)).abs() < 1e-12);
        assert!((princarg(0.5) - 0.5).abs() < 1e-12);
    }
}
