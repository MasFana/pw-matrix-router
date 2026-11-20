//! Stateful stereo meters and Go-compatible 16-band Hann/Goertzel analysis.
//!
//! Spectra are normalized display magnitudes, not linear amplitudes. Analysis
//! uses at most the first 256 frames, while peak/RMS/correlation use the full block.

use std::f64::consts::TAU;

const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const BANDS: usize = 16;
const MAX_SPECTRUM_FRAMES: usize = 256;

/// Complete, copyable state of the stereo telemetry display.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct TelemetrySnapshot {
    pub peak_l: f64,
    pub peak_r: f64,
    pub peak_hold_l: f64,
    pub peak_hold_r: f64,
    pub rms_l: f64,
    pub rms_r: f64,
    pub correlation: f64,
    pub balance: f64,
    /// Go display normalization: clamp((20 log10(magnitude * 28 + 1e-6) + 42) / 42).
    pub spectrum: [f64; BANDS],
    pub spec_hold: [f64; BANDS],
    pub sample_rate: u32,
    pub channels: u32,
    pub bits: u32,
    pub period_ms: f64,
}

/// A single capture stream's analyzer. Streaming operations allocate nothing.
pub struct TelemetryAnalyzer {
    snapshot: TelemetrySnapshot,
    coefficients: [f64; BANDS],
    // Cache the current block length in fixed storage. Capture normally uses
    // 256 frames; a short block only rebuilds its window when the length changes.
    window: [f64; MAX_SPECTRUM_FRAMES],
    window_frames: usize,
}

impl TelemetryAnalyzer {
    pub fn new(sr: u32) -> Self {
        let sr = if sr == 0 { DEFAULT_SAMPLE_RATE } else { sr };
        let mut coefficients = [0.0; BANDS];
        for (band, coefficient) in coefficients.iter_mut().enumerate() {
            let frequency = 45.0 * 1.46_f64.powi(band as i32);
            *coefficient = 2.0 * (TAU * frequency / f64::from(sr)).cos();
        }
        let mut window = [0.0; MAX_SPECTRUM_FRAMES];
        for (i, value) in window.iter_mut().enumerate() {
            *value = 0.5 * (1.0 - (TAU * i as f64 / MAX_SPECTRUM_FRAMES as f64).cos());
        }
        Self {
            snapshot: TelemetrySnapshot {
                sample_rate: sr,
                channels: 2,
                bits: 32,
                ..TelemetrySnapshot::default()
            },
            coefficients,
            window,
            window_frames: MAX_SPECTRUM_FRAMES,
        }
    }

    /// Analyze complete interleaved stereo frames, retaining Go meter smoothing.
    /// Empty input (or a lone sample) leaves the prior snapshot unchanged.
    pub fn update(&mut self, stereo: &[f32]) -> TelemetrySnapshot {
        let frames = stereo.len() / 2;
        if frames == 0 {
            return self.snapshot;
        }
        let spectrum_frames = frames.min(MAX_SPECTRUM_FRAMES);
        if self.window_frames != spectrum_frames {
            for (i, value) in self.window[..spectrum_frames].iter_mut().enumerate() {
                *value = 0.5 * (1.0 - (TAU * i as f64 / spectrum_frames as f64).cos());
            }
            self.window_frames = spectrum_frames;
        }
        let window = &self.window[..spectrum_frames];
        let mut q1 = [0.0; BANDS];
        let mut q2 = [0.0; BANDS];
        let mut sum_l = 0.0;
        let mut sum_r = 0.0;
        let mut dot = 0.0;
        let mut peak_l = 0.0_f64;
        let mut peak_r = 0.0_f64;

        for (i, frame) in stereo.chunks_exact(2).enumerate() {
            let l = clean_sample(frame[0]);
            let r = clean_sample(frame[1]);
            let l64 = f64::from(l);
            let r64 = f64::from(r);
            peak_l = peak_l.max(l64.abs());
            peak_r = peak_r.max(r64.abs());
            sum_l += l64 * l64;
            sum_r += r64 * r64;
            dot += l64 * r64;
            if i < spectrum_frames {
                // Match Go's float32 stereo sum for ordinary audio, while avoiding
                // overflow if a caller supplies finite samples beyond audio range.
                let sum = l + r;
                let mono = if sum.is_finite() {
                    f64::from(sum)
                } else {
                    l64 + r64
                };
                let sample = mono * 0.5 * window[i];
                for band in 0..BANDS {
                    let q0 = self.coefficients[band] * q1[band] - q2[band] + sample;
                    q2[band] = q1[band];
                    q1[band] = q0;
                }
            }
        }

        let rms_l = (sum_l / frames as f64).sqrt();
        let rms_r = (sum_r / frames as f64).sqrt();
        let denominator = (sum_l * sum_r).sqrt();
        let correlation = if denominator > 1e-9 {
            (dot / denominator).clamp(-1.0, 1.0)
        } else {
            1.0
        };
        let balance = if rms_l + rms_r > 1e-6 {
            ((rms_r - rms_l) / (rms_l + rms_r)).clamp(-1.0, 1.0)
        } else {
            0.0
        };

        let state = &mut self.snapshot;
        state.period_ms = frames as f64 * 1000.0 / f64::from(state.sample_rate);
        state.peak_l = peak_l.max(state.peak_l * 0.80);
        state.peak_r = peak_r.max(state.peak_r * 0.80);
        state.peak_hold_l = peak_l.max(state.peak_hold_l * 0.975);
        state.peak_hold_r = peak_r.max(state.peak_hold_r * 0.975);
        state.rms_l = rms_l * 0.55 + state.rms_l * 0.45;
        state.rms_r = rms_r * 0.55 + state.rms_r * 0.45;
        state.correlation = correlation * 0.3 + state.correlation * 0.7;
        state.balance = balance * 0.3 + state.balance * 0.7;
        for band in 0..BANDS {
            let power = (q1[band] * q1[band] + q2[band] * q2[band]
                - q1[band] * q2[band] * self.coefficients[band])
                .max(0.0);
            let magnitude = power.sqrt() / spectrum_frames as f64;
            let db = 20.0 * (magnitude * 28.0 + 1e-6).log10();
            let level = ((db + 42.0) / 42.0).clamp(0.0, 1.0);
            state.spectrum[band] = level.max(state.spectrum[band] * 0.76);
            state.spec_hold[band] = if level >= state.spec_hold[band] {
                level
            } else {
                (state.spec_hold[band] - 0.018).max(0.0)
            };
        }
        *state
    }

    /// Apply Go's idle decay; format and stereo relationship remain unchanged.
    pub fn decay(&mut self) -> TelemetrySnapshot {
        let state = &mut self.snapshot;
        state.peak_l *= 0.72;
        state.peak_r *= 0.72;
        state.peak_hold_l *= 0.94;
        state.peak_hold_r *= 0.94;
        state.rms_l *= 0.72;
        state.rms_r *= 0.72;
        for band in 0..BANDS {
            state.spectrum[band] *= 0.75;
            state.spec_hold[band] = (state.spec_hold[band] - 0.025).max(0.0);
        }
        *state
    }
}

fn clean_sample(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(sr: u32, frequency: f64, amplitude: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let sample = (TAU * frequency * i as f64 / f64::from(sr)).sin() as f32 * amplitude;
                [sample, sample]
            })
            .collect()
    }

    // Direct transliteration of Go's per-band loop, independent of the packed
    // window table and the frame-major streaming implementation.
    fn go_spectrum(stereo: &[f32], sr: u32) -> [f64; BANDS] {
        let frames = (stereo.len() / 2).min(MAX_SPECTRUM_FRAMES);
        let mut bands = [0.0; BANDS];
        for (band, level) in bands.iter_mut().enumerate() {
            let frequency = 45.0 * 1.46_f64.powf(band as f64);
            let coefficient = 2.0 * (TAU * frequency / f64::from(sr)).cos();
            let mut q1 = 0.0;
            let mut q2 = 0.0;
            for i in 0..frames {
                let window = 0.5 * (1.0 - (TAU * i as f64 / frames as f64).cos());
                let mono = f64::from(stereo[i * 2] + stereo[i * 2 + 1]) * 0.5 * window;
                let q0 = coefficient * q1 - q2 + mono;
                q2 = q1;
                q1 = q0;
            }
            let magnitude =
                (q1 * q1 + q2 * q2 - q1 * q2 * coefficient).max(0.0).sqrt() / frames as f64;
            *level = ((20.0 * (magnitude * 28.0 + 1e-6).log10() + 42.0) / 42.0).clamp(0.0, 1.0);
        }
        bands
    }

    #[test]
    fn spectrum_matches_go_hann_goertzel_for_every_block_shape() {
        for sr in [44_100, 48_000, 96_000] {
            for frames in [1, 7, 64, 255, 256, 300] {
                let input = tone(sr, 1234.0, 0.02, frames);
                let expected = go_spectrum(&input, sr);
                let actual = TelemetryAnalyzer::new(sr).update(&input);
                for band in 0..BANDS {
                    assert!(
                        (actual.spectrum[band] - expected[band]).abs() < 1e-9,
                        "sample rate {sr}, length {frames}, band {band}"
                    );
                }
            }
        }
    }

    #[test]
    fn separated_tones_select_their_frequency_bands_not_all_bands() {
        for selected in [9, 12, 15] {
            let frequency = 45.0 * 1.46_f64.powi(selected as i32);
            let input = tone(48_000, frequency, 0.02, 256);
            let snapshot = TelemetryAnalyzer::new(48_000).update(&input);
            let strongest = snapshot
                .spectrum
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .unwrap()
                .0;
            assert_eq!(strongest, selected);
            assert!(snapshot.spectrum[selected] > 0.5);
            let distant = if selected == 9 { 15 } else { 9 };
            assert!(snapshot.spectrum[selected] - snapshot.spectrum[distant] > 0.4);
            assert!(
                snapshot
                    .spectrum
                    .iter()
                    .all(|value| (0.0..=1.0).contains(value))
            );
        }
    }

    #[test]
    fn stereo_correlation_distinguishes_in_phase_antiphase_and_quadrature() {
        let input = tone(48_000, 1500.0, 0.5, 256);
        let anti: Vec<f32> = input
            .chunks_exact(2)
            .flat_map(|frame| [frame[0], -frame[1]])
            .collect();
        let quadrature: Vec<f32> = (0..256)
            .flat_map(|i| {
                let angle = TAU * 8.0 * i as f64 / 256.0;
                [angle.sin() as f32 * 0.5, angle.cos() as f32 * 0.5]
            })
            .collect();
        let mut same_analyzer = TelemetryAnalyzer::new(48_000);
        let mut anti_analyzer = TelemetryAnalyzer::new(48_000);
        let mut quadrature_analyzer = TelemetryAnalyzer::new(48_000);
        for _ in 0..32 {
            same_analyzer.update(&input);
            anti_analyzer.update(&anti);
            quadrature_analyzer.update(&quadrature);
        }
        assert!(same_analyzer.snapshot.correlation > 0.9999);
        assert!(anti_analyzer.snapshot.correlation < -0.9999);
        assert!(quadrature_analyzer.snapshot.correlation.abs() < 1e-7);
        assert_eq!(anti_analyzer.snapshot.spectrum, [0.0; BANDS]);
        assert!(same_analyzer.snapshot.balance.abs() < 1e-12);
    }

    #[test]
    fn full_stereo_fields_and_stateful_meter_hold_and_idle_decay_match_go() {
        let mut analyzer = TelemetryAnalyzer::new(0);
        let input = [0.5, -0.25].repeat(256);
        let first = analyzer.update(&input);
        assert_eq!(first.sample_rate, 48_000);
        assert_eq!(first.channels, 2);
        assert_eq!(first.bits, 32);
        assert!((first.period_ms - 256.0 / 48.0).abs() < 1e-12);
        assert_eq!(first.peak_l, 0.5);
        assert_eq!(first.peak_r, 0.25);
        assert_eq!(first.peak_hold_l, 0.5);
        assert_eq!(first.peak_hold_r, 0.25);
        assert!((first.rms_l - 0.275).abs() < 1e-12);
        assert!((first.rms_r - 0.1375).abs() < 1e-12);
        assert!((first.correlation + 0.3).abs() < 1e-12);
        assert!((first.balance + 0.1).abs() < 1e-12);
        let quiet = analyzer.update(&[0.0; 512]);
        assert!((quiet.peak_l - first.peak_l * 0.80).abs() < 1e-12);
        assert!((quiet.peak_hold_r - first.peak_hold_r * 0.975).abs() < 1e-12);
        assert!((quiet.rms_l - first.rms_l * 0.45).abs() < 1e-12);
        assert!((quiet.correlation - (first.correlation * 0.7 + 0.3)).abs() < 1e-12);
        assert!((quiet.balance - first.balance * 0.7).abs() < 1e-12);
        for band in 0..BANDS {
            assert!((quiet.spectrum[band] - first.spectrum[band] * 0.76).abs() < 1e-12);
            assert!(
                (quiet.spec_hold[band] - (first.spec_hold[band] - 0.018).max(0.0)).abs() < 1e-12
            );
        }
        let idle = analyzer.decay();
        assert!((idle.peak_l - quiet.peak_l * 0.72).abs() < 1e-12);
        assert!((idle.peak_hold_r - quiet.peak_hold_r * 0.94).abs() < 1e-12);
        assert!((idle.rms_r - quiet.rms_r * 0.72).abs() < 1e-12);
        assert_eq!(idle.correlation, quiet.correlation);
        assert_eq!(idle.balance, quiet.balance);
        assert_eq!(idle.period_ms, quiet.period_ms);
        for band in 0..BANDS {
            assert!((idle.spectrum[band] - quiet.spectrum[band] * 0.75).abs() < 1e-12);
            assert!(
                (idle.spec_hold[band] - (quiet.spec_hold[band] - 0.025).max(0.0)).abs() < 1e-12
            );
        }
    }

    #[test]
    fn one_sided_audio_reports_channel_balance_and_full_block_rms() {
        let mut analyzer = TelemetryAnalyzer::new(48_000);
        let first = analyzer.update(&[0.0, 0.5].repeat(32));
        assert_eq!(first.peak_l, 0.0);
        assert_eq!(first.peak_r, 0.5);
        assert!((first.balance - 0.3).abs() < 1e-12);
        assert!((first.rms_r - 0.275).abs() < 1e-12);
        let mut block = vec![0.0; 512];
        block.extend_from_slice(&[0.5, 0.5].repeat(256));
        let snapshot = TelemetryAnalyzer::new(48_000).update(&block);
        assert_eq!(snapshot.peak_l, 0.5);
        assert!((snapshot.rms_l - (0.125_f64).sqrt() * 0.55).abs() < 1e-12);
        assert_eq!(snapshot.spectrum, [0.0; BANDS]);
        assert!((snapshot.period_ms - 512.0 / 48.0).abs() < 1e-12);
    }

    #[test]
    fn empty_partial_and_poisoned_blocks_are_conservative() {
        let mut analyzer = TelemetryAnalyzer::new(48_000);
        let initial = analyzer.update(&[]);
        assert_eq!(initial.sample_rate, 48_000);
        assert_eq!(initial.peak_l, 0.0);
        let snapshot = analyzer.update(&[0.5, 0.25]);
        assert_eq!(analyzer.update(&[]), snapshot);
        assert_eq!(analyzer.update(&[f32::NAN]), snapshot);
        let clean = analyzer.update(&[f32::NAN, 0.5, f32::INFINITY, f32::NEG_INFINITY]);
        for value in [
            clean.peak_l,
            clean.peak_r,
            clean.peak_hold_l,
            clean.peak_hold_r,
            clean.rms_l,
            clean.rms_r,
            clean.correlation,
            clean.balance,
            clean.period_ms,
        ] {
            assert!(value.is_finite());
        }
        assert!(
            clean
                .spectrum
                .iter()
                .chain(clean.spec_hold.iter())
                .all(|value| value.is_finite())
        );
        assert!(
            clean
                .spectrum
                .iter()
                .all(|value| (0.0..=1.0).contains(value))
        );
    }
}
