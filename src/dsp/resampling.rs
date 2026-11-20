//! Causal, continuously variable-ratio windowed-sinc sample-rate conversion.
//!
//! The 128-tap Blackman window trades CPU and a 63-source-frame delay for
//! anti-alias filtering. Its cutoff is 95% of the lower Nyquist frequency;
//! transition width remains fixed in source-frequency units, so very large
//! downsampling ratios have less rejection near the destination Nyquist limit.
//! See Julius O. Smith, https://ccrma.stanford.edu/~jos/resample/Implementation.html.
//! Prototype lookup tables are shared and initialized before audio streaming.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

pub(crate) const TAPS: usize = 128;
pub(crate) const HISTORY_FRAMES: usize = TAPS - 1;
pub(crate) const DELAY_FRAMES: usize = TAPS / 2 - 1;
const TABLE_INTERVALS: usize = 8192;
const SUPPORT: f32 = TAPS as f32 / 2.0;
const TABLE_SCALE: f32 = TABLE_INTERVALS as f32 / SUPPORT;

pub(crate) struct SincTable {
    sinc: Box<[f32]>,
    window: Box<[f32]>,
}

impl SincTable {
    pub(crate) fn shared() -> &'static Self {
        static TABLE: LazyLock<SincTable> = LazyLock::new(|| {
            let mut sinc = Vec::with_capacity(TABLE_INTERVALS + 1);
            let mut window = Vec::with_capacity(TABLE_INTERVALS + 1);
            for index in 0..=TABLE_INTERVALS {
                let distance = index as f64 * f64::from(SUPPORT) / TABLE_INTERVALS as f64;
                let angle = std::f64::consts::PI * distance;
                sinc.push(if index == 0 {
                    1.0
                } else {
                    (angle.sin() / angle) as f32
                });
                let phase = angle / f64::from(SUPPORT);
                window.push((0.42 + 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos()) as f32);
            }
            SincTable {
                sinc: sinc.into_boxed_slice(),
                window: window.into_boxed_slice(),
            }
        });
        &TABLE
    }

    /// Conservative upper servo ratio prevents cutoff modulation with buffer fill.
    pub(crate) fn cutoff(base_ratio: f64) -> f32 {
        (0.95 / (base_ratio * 1.0018).max(1.0)) as f32
    }

    fn lookup(table: &[f32], distance: f32) -> f32 {
        let position = (distance.abs() * TABLE_SCALE).min(TABLE_INTERVALS as f32);
        let index = position as usize;
        if index == TABLE_INTERVALS {
            return table[index];
        }
        let fraction = position - index as f32;
        table[index] + fraction * (table[index + 1] - table[index])
    }

    #[cfg(test)]
    /// Both channels share each coefficient and DC normalization.
    pub(crate) fn interpolate(
        &self,
        fraction: f32,
        cutoff: f32,
        mut sample: impl FnMut(usize) -> [f32; 2],
    ) -> [f32; 2] {
        let mut sum = [0.0; 2];
        let mut weight = 0.0;
        for tap in 0..TAPS {
            let distance = tap as f32 + fraction - DELAY_FRAMES as f32;
            let coefficient =
                Self::lookup(&self.sinc, distance * cutoff) * Self::lookup(&self.window, distance);
            let frame = sample(tap);
            sum[0] += frame[0] * coefficient;
            sum[1] += frame[1] * coefficient;
            weight += coefficient;
        }
        [sum[0] / weight, sum[1] / weight]
    }
}

const PHASES: usize = 1024;

/// Rate-specific coefficients are shared by outputs and prepared before playback.
pub(crate) struct PhaseTable {
    coefficients: Box<[[f32; TAPS]]>,
}

impl PhaseTable {
    pub(crate) fn shared(source_rate: u32, destination_rate: u32) -> Arc<Self> {
        type Cache = HashMap<(u32, u32), Weak<PhaseTable>>;
        static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(HashMap::new()));
        let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
        let key = (source_rate, destination_rate);
        if let Some(table) = cache.get(&key).and_then(Weak::upgrade) {
            return table;
        }
        cache.retain(|_, table| table.strong_count() != 0);
        let prototype = SincTable::shared();
        let cutoff = SincTable::cutoff(f64::from(source_rate) / f64::from(destination_rate));
        let mut coefficients = Vec::with_capacity(PHASES + 1);
        for phase in 0..=PHASES {
            let fraction = phase as f32 / PHASES as f32;
            let mut row = [0.; TAPS];
            let mut weight = 0.;
            for (tap, value) in row.iter_mut().enumerate() {
                let distance = tap as f32 + fraction - DELAY_FRAMES as f32;
                *value = SincTable::lookup(&prototype.sinc, distance * cutoff)
                    * SincTable::lookup(&prototype.window, distance);
                weight += *value;
            }
            for value in &mut row {
                *value /= weight;
            }
            coefficients.push(row);
        }
        let table = Arc::new(Self {
            coefficients: coefficients.into_boxed_slice(),
        });
        cache.insert(key, Arc::downgrade(&table));
        table
    }

    pub(crate) fn interpolate(
        &self,
        fraction: f32,
        mut sample: impl FnMut(usize) -> [f32; 2],
    ) -> [f32; 2] {
        let position = fraction * PHASES as f32;
        let phase = (position as usize).min(PHASES - 1);
        let blend = position - phase as f32;
        let current = &self.coefficients[phase];
        let next = &self.coefficients[phase + 1];
        let mut sum = [0.; 2];
        for tap in 0..TAPS {
            let coefficient = current[tap] + blend * (next[tap] - current[tap]);
            let frame = sample(tap);
            sum[0] += frame[0] * coefficient;
            sum[1] += frame[1] * coefficient;
        }
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_signal_and_stereo_symmetry_survive_fractional_phases() {
        let table = SincTable::shared();
        for ratio in [0.5, 1.0, 48_000.0 / 44_100.0, 2.0, 4.0, 1000.0] {
            for phase in 0..=128 {
                let out = table.interpolate(phase as f32 / 128.0, SincTable::cutoff(ratio), |_| {
                    [0.25, -0.25]
                });
                assert!((out[0] - 0.25).abs() < 1e-6);
                assert_eq!(out[0], -out[1]);
            }
        }
    }

    fn tone_amplitude(input_frequency: f64, ratio: f64) -> f64 {
        let table = SincTable::shared();
        let mut energy = 0.0;
        let frames = 4096;
        for frame in 0..frames {
            let position = 200.0 + frame as f64 * ratio;
            let offset = position.floor();
            let out = table.interpolate(
                (position - offset) as f32,
                SincTable::cutoff(ratio),
                |tap| {
                    let value = (std::f64::consts::TAU * input_frequency * (offset - tap as f64))
                        .sin() as f32;
                    [value, value]
                },
            );
            energy += f64::from(out[0]).powi(2);
        }
        (2.0 * energy / frames as f64).sqrt()
    }

    #[test]
    fn passband_tones_keep_amplitude_at_common_rate_ratios() {
        for ratio in [0.5_f64, 1.0, 48_000.0 / 44_100.0, 2.0, 4.0] {
            let amplitude = tone_amplitude(0.10 / ratio.max(1.0), ratio);
            assert!(
                (amplitude - 1.0).abs() < 0.002,
                "ratio={ratio}, amplitude={amplitude}"
            );
        }
    }

    #[test]
    fn twenty_khz_passband_remains_within_point_two_db() {
        for (source_rate, destination_rate) in [(44_100.0, 44_100.0), (48_000.0, 44_100.0)] {
            let ratio = source_rate / destination_rate;
            let amplitude = tone_amplitude(20_000.0 / source_rate, ratio);
            let error_db = 20.0 * amplitude.log10();
            assert!(error_db.abs() < 0.2, "ratio={ratio}, error_db={error_db}");
        }
    }

    #[test]
    fn twenty_three_khz_alias_is_rejected_at_forty_four_point_one_khz() {
        let amplitude = tone_amplitude(23_000.0 / 48_000.0, 48_000.0 / 44_100.0);
        assert!(amplitude < 0.001, "amplitude={amplitude}");
    }

    #[test]
    fn downsampling_rejects_out_of_band_tones() {
        // 18 kHz at 48 -> 24 kHz and 60 kHz at 192 -> 48 kHz fold without filtering.
        for (frequency, ratio) in [(0.375, 2.0), (0.3125, 4.0)] {
            let amplitude = tone_amplitude(frequency, ratio);
            assert!(amplitude < 0.001, "ratio={ratio}, amplitude={amplitude}");
        }
    }

    #[test]
    fn impulse_peak_matches_declared_source_frame_delay() {
        let table = SincTable::shared();
        let output: Vec<f32> = (0..TAPS)
            .map(|frame| {
                table.interpolate(0.0, SincTable::cutoff(1.0), |tap| {
                    let value = if frame == tap { 1.0 } else { 0.0 };
                    [value, value]
                })[0]
            })
            .collect();
        let peak = output
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.total_cmp(right.1))
            .unwrap()
            .0;
        assert_eq!(peak, DELAY_FRAMES);
    }
}
