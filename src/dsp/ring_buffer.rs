//! Split single-producer/single-consumer stereo transport and drift servo.
//!
//! Only the consumer releases occupied frames. Sample words are atomic, and
//! release/acquire cursor publication protects complete stereo frames. Filter
//! history stays consumer-local after its ring slots are freed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[cfg(test)]
use super::resampling::SincTable;
use super::resampling::{DELAY_FRAMES, HISTORY_FRAMES, PhaseTable, TAPS};
use super::volume::soft_limit;

const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const GAIN_TIME_SECONDS: f64 = 0.014;

/// Causal FIR signal delay, in source frames, separate from unread ring fill.
pub const RESAMPLER_DELAY_FRAMES: usize = DELAY_FRAMES;

// Producer and consumer publish on separate cache lines. Alignment changes no
// ownership or memory ordering; release/acquire still protects stereo frames.
#[repr(align(64))]
struct PublishedCursor(AtomicU64);

impl PublishedCursor {
    fn load(&self, ordering: Ordering) -> u64 {
        self.0.load(ordering)
    }

    fn store(&self, position: u64, ordering: Ordering) {
        self.0.store(position, ordering);
    }
}

struct SharedRing {
    samples: Box<[AtomicU32]>,
    capacity: usize,
    write_pos: PublishedCursor,
    read_pos: PublishedCursor,
    underruns: AtomicU64,
    overruns: AtomicU64,
}

/// The unique writer. Excess input is rejected, never substituted for unread audio.
pub struct RingProducer {
    shared: Arc<SharedRing>,
    write_pos: u64,
    write_index: usize,
}

/// The unique reader, including all interpolation and gain-smoothing state.
pub struct RingConsumer {
    shared: Arc<SharedRing>,
    read_pos: u64,
    read_index: usize,
    history: [[f32; 2]; HISTORY_FRAMES],
    history_initialized: bool,
    filter: Option<Arc<PhaseTable>>,
    filter_rates: Option<(u32, u32)>,
    current_gain: f32,
    gain_rate: u32,
    gain_alpha: f32,
    resample_pos: f64,
    smoothed_fill: f64,
    smoothed_ratio: f64,
    primed: bool,
}

/// Read-only, cheaply clonable transport statistics.
#[derive(Clone)]
pub struct RingMonitor {
    shared: Arc<SharedRing>,
}

/// Create a stereo ring with exactly one producer and one consumer.
///
/// A zero-capacity ring is disabled: writes reject input and reads produce silence.
/// Neither owning handle is clonable; mutation requires an exclusive borrow.
/// Transport allocation happens here; call `prepare_rates` before live playback.
pub fn stereo_ring(capacity_frames: usize) -> (RingProducer, RingConsumer, RingMonitor) {
    let sample_count = capacity_frames
        .checked_mul(2)
        .expect("stereo capacity overflow");
    let shared = Arc::new(SharedRing {
        samples: (0..sample_count).map(|_| AtomicU32::new(0)).collect(),
        capacity: capacity_frames,
        write_pos: PublishedCursor(AtomicU64::new(0)),
        read_pos: PublishedCursor(AtomicU64::new(0)),
        underruns: AtomicU64::new(0),
        overruns: AtomicU64::new(0),
    });
    let producer = RingProducer {
        shared: Arc::clone(&shared),
        write_pos: 0,
        write_index: 0,
    };
    let consumer = RingConsumer {
        shared: Arc::clone(&shared),
        read_pos: 0,
        read_index: 0,
        history: [[0.0; 2]; HISTORY_FRAMES],
        history_initialized: false,
        filter: None,
        filter_rates: None,
        current_gain: 1.0,
        gain_rate: DEFAULT_SAMPLE_RATE,
        gain_alpha: gain_alpha(DEFAULT_SAMPLE_RATE),
        resample_pos: 0.0,
        smoothed_fill: 0.0,
        smoothed_ratio: 0.0,
        primed: false,
    };
    (producer, consumer, RingMonitor { shared })
}

impl RingProducer {
    /// Publish complete interleaved frames, returning the number accepted.
    ///
    /// An odd trailing sample is ignored. A write that cannot fit increments the
    /// overrun count once and rejects its newest excess. It never moves the reader.
    pub fn write_stereo(&mut self, stereo: &[f32]) -> usize {
        let frames = stereo.len() / 2;
        let read_pos = self.shared.read_pos.load(Ordering::Acquire);
        let occupied = self.write_pos.wrapping_sub(read_pos) as usize;
        let writable = self.shared.capacity.saturating_sub(occupied);
        let written = frames.min(writable);
        if written < frames {
            self.shared.overruns.fetch_add(1, Ordering::Relaxed);
        }
        if written == 0 {
            return 0;
        }

        let mut index = self.write_index;
        for frame in stereo[..written * 2].chunks_exact(2) {
            self.shared.samples[index * 2]
                .store(finite_sample(frame[0]).to_bits(), Ordering::Relaxed);
            self.shared.samples[index * 2 + 1]
                .store(finite_sample(frame[1]).to_bits(), Ordering::Relaxed);
            index += 1;
            if index == self.shared.capacity {
                index = 0;
            }
        }
        self.write_index = index;
        self.write_pos = self.write_pos.wrapping_add(written as u64);
        // Publish only after both sample words in every accepted frame are stored.
        self.shared
            .write_pos
            .store(self.write_pos, Ordering::Release);
        written
    }
}

impl RingConsumer {
    /// Prepare shared filter coefficients outside the audio-delivery loop.
    /// Zero rates use 48 kHz. Repeated preparation for unchanged rates is free.
    pub fn prepare_rates(&mut self, source_rate: u32, destination_rate: u32) {
        let source_rate = if source_rate == 0 {
            DEFAULT_SAMPLE_RATE
        } else {
            source_rate
        };
        let destination_rate = if destination_rate == 0 {
            DEFAULT_SAMPLE_RATE
        } else {
            destination_rate
        };
        let rates = (source_rate, destination_rate);
        if self.filter_rates != Some(rates) {
            self.filter = Some(PhaseTable::shared(source_rate, destination_rate));
            self.filter_rates = Some(rates);
        }
    }

    /// Discard currently published audio and re-prime without resetting gain.
    pub fn reset(&mut self) {
        let write_pos = self.shared.write_pos.load(Ordering::Acquire);
        let discarded = write_pos.wrapping_sub(self.read_pos) as usize;
        self.advance(discarded);
        self.clear_interpolation();
    }

    /// Render all complete destination frames, or silence and return zero.
    ///
    /// Initial/recovery priming is not an underrun. A previously running stream
    /// that lacks a full callback's source audio counts one underrun and re-primes.
    /// Zero sample rates use 48 kHz; an odd destination tail is always silenced.
    pub fn read_stereo_adaptive(
        &mut self,
        dst: &mut [f32],
        gain: f32,
        muted: bool,
        src_rate: u32,
        dst_rate: u32,
        latency_frames: usize,
    ) -> usize {
        let out_frames = dst.len() / 2;
        if dst.len() % 2 != 0 {
            dst[out_frames * 2] = 0.0;
        }
        if out_frames == 0 || self.shared.capacity == 0 {
            dst.fill(0.0);
            return 0;
        }
        let src_rate = if src_rate == 0 {
            DEFAULT_SAMPLE_RATE
        } else {
            src_rate
        };
        let dst_rate = if dst_rate == 0 {
            DEFAULT_SAMPLE_RATE
        } else {
            dst_rate
        };
        self.prepare_rates(src_rate, dst_rate);
        if self.gain_rate != dst_rate {
            self.gain_rate = dst_rate;
            self.gain_alpha = gain_alpha(dst_rate);
        }
        let base_ratio = f64::from(src_rate) / f64::from(dst_rate);
        let target = latency_frames.min(self.shared.capacity);
        let write_pos = self.shared.write_pos.load(Ordering::Acquire);
        let mut available = write_pos.wrapping_sub(self.read_pos) as usize;
        let needed_min = ((out_frames as f64 * base_ratio).ceil() as usize).saturating_add(4);

        if !self.primed {
            // Division first avoids overflowing 4 * target at extreme capacities.
            let prime_threshold = target / 5 * 4 + target % 5 * 4 / 5;
            if available < prime_threshold || available < needed_min {
                dst.fill(0.0);
                return 0;
            }
            self.primed = true;
            self.smoothed_fill = available as f64;
            self.smoothed_ratio = base_ratio;
        }

        let slack = (u64::from(src_rate) * 85 / 1000) as usize;
        if available > target.saturating_add(slack) {
            // Do not snap below a full callback plus its interpolation guard.
            let retained = target.max(needed_min).min(available);
            let discarded = available - retained;
            if discarded != 0 {
                self.retain_history(discarded);
                self.advance(discarded);
                available = retained;
                self.smoothed_fill = available as f64;
            }
        }

        if available < needed_min {
            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
            self.clear_interpolation();
            dst.fill(0.0);
            return 0;
        }

        self.smoothed_fill = self.smoothed_fill * 0.96 + available as f64 * 0.04;
        let drift = ((self.smoothed_fill - target as f64) * 0.0000015).clamp(-0.0018, 0.0018);
        let target_ratio = base_ratio * (1.0 + drift);
        self.smoothed_ratio = self.smoothed_ratio * 0.92 + target_ratio * 0.08;
        let mut step_ratio = self.smoothed_ratio;
        // Include carried fractional phase when bounding the interpolation lookahead.
        let max_needed = ((self.resample_pos + out_frames as f64 * step_ratio).ceil() as usize)
            .saturating_add(3);
        if max_needed > available {
            step_ratio =
                (available.saturating_sub(3) as f64 - self.resample_pos) / out_frames as f64;
        }

        if !self.history_initialized {
            let first = self.load_frame(0);
            self.history.fill(first);
            self.history_initialized = true;
        }
        let filter = self.filter.as_ref().expect("prepared filter");
        let mut window = [[0.; 2]; TAPS];
        window[0] = self.load_frame(0);
        for (tap, frame) in window.iter_mut().enumerate().skip(1) {
            *frame = self.history[HISTORY_FRAMES - tap];
        }
        let mut head = 0;
        let mut loaded_offset = 0;
        let goal_gain = if muted || !gain.is_finite() {
            0.0
        } else {
            gain
        };
        let mut current_gain = self.current_gain;
        let mut pos = self.resample_pos;
        for frame in dst[..out_frames * 2].chunks_exact_mut(2) {
            current_gain += self.gain_alpha * (goal_gain - current_gain);
            let offset = pos as usize;
            let frac = (pos - offset as f64) as f32;
            while loaded_offset < offset {
                loaded_offset += 1;
                head = (head + TAPS - 1) & (TAPS - 1);
                window[head] = self.load_frame(loaded_offset);
            }
            let filtered = filter.interpolate(frac, |tap| window[(head + tap) & (TAPS - 1)]);
            frame[0] = soft_limit(filtered[0] * current_gain);
            frame[1] = soft_limit(filtered[1] * current_gain);
            pos += step_ratio;
        }

        let consumed = pos as usize;
        // Preserve history before publishing free slots; producer can reuse them immediately.
        if consumed != 0 {
            while loaded_offset + 1 < consumed {
                loaded_offset += 1;
                head = (head + TAPS - 1) & (TAPS - 1);
                window[head] = self.load_frame(loaded_offset);
            }
            let newest = loaded_offset + 1 - consumed;
            for (index, frame) in self.history.iter_mut().enumerate() {
                *frame = window[(head + newest + HISTORY_FRAMES - 1 - index) & (TAPS - 1)];
            }
        }
        self.resample_pos = pos - consumed as f64;
        self.current_gain = current_gain;
        self.advance(consumed);
        out_frames
    }

    fn load_frame(&self, offset: usize) -> [f32; 2] {
        // Every offset addresses this callback's published occupied frames and
        // is smaller than capacity, so at most one wrap is possible.
        let slot = self.read_index + offset;
        let slot = if slot >= self.shared.capacity {
            slot - self.shared.capacity
        } else {
            slot
        };
        let index = slot * 2;
        [
            f32::from_bits(self.shared.samples[index].load(Ordering::Relaxed)),
            f32::from_bits(self.shared.samples[index + 1].load(Ordering::Relaxed)),
        ]
    }

    fn retain_history(&mut self, consumed: usize) {
        let retained = consumed.min(HISTORY_FRAMES);
        if retained == 0 {
            return;
        }
        if !self.history_initialized {
            let first = self.load_frame(consumed - retained);
            self.history.fill(first);
            self.history_initialized = true;
        }
        self.history.copy_within(retained.., 0);
        for index in 0..retained {
            self.history[HISTORY_FRAMES - retained + index] =
                self.load_frame(consumed - retained + index);
        }
    }

    fn advance(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        if self.shared.capacity != 0 {
            self.read_index = (self.read_index + frames) % self.shared.capacity;
        }
        self.read_pos = self.read_pos.wrapping_add(frames as u64);
        self.shared.read_pos.store(self.read_pos, Ordering::Release);
    }

    fn clear_interpolation(&mut self) {
        self.history.fill([0.0; 2]);
        self.history_initialized = false;
        self.resample_pos = 0.0;
        self.smoothed_fill = 0.0;
        self.smoothed_ratio = 0.0;
        self.primed = false;
    }
}

impl RingMonitor {
    /// Approximate concurrent occupancy, bounded by capacity.
    pub fn available_read(&self) -> usize {
        // Read first: its acquire also observes the write publication it consumed.
        let read_pos = self.shared.read_pos.load(Ordering::Acquire);
        let write_pos = self.shared.write_pos.load(Ordering::Acquire);
        (write_pos.wrapping_sub(read_pos) as usize).min(self.shared.capacity)
    }

    pub fn underrun_count(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    pub fn overrun_count(&self) -> u64 {
        self.shared.overruns.load(Ordering::Relaxed)
    }
}

fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

fn gain_alpha(sample_rate: u32) -> f32 {
    (-(-1.0 / (GAIN_TIME_SECONDS * f64::from(sample_rate))).exp_m1()) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| [i as f32 * 0.01, -(i as f32) * 0.01])
            .collect()
    }

    fn assert_filtered_sequence(output: &[f32], input: &[f32], start: f64, ratio: f64, step: f64) {
        let table = SincTable::shared();
        for (index, frame) in output.chunks_exact(2).enumerate() {
            let position = start + index as f64 * step;
            let offset = position.floor() as usize;
            let expected = table.interpolate(
                (position - offset as f64) as f32,
                SincTable::cutoff(ratio),
                |tap| {
                    let source = offset.saturating_sub(tap) * 2;
                    [input[source], input[source + 1]]
                },
            );
            assert!((frame[0] - expected[0]).abs() < 1e-6);
            assert!((frame[1] - expected[1]).abs() < 1e-6);
        }
    }

    #[test]
    fn zero_capacity_and_short_buffers_are_silent() {
        let (mut producer, mut consumer, monitor) = stereo_ring(0);
        assert_eq!(producer.write_stereo(&[0.5, -0.5]), 0);
        assert_eq!(monitor.overrun_count(), 1);
        let mut out = [1.0; 3];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 0, 0, 0),
            0
        );
        assert_eq!(out, [0.0; 3]);
        consumer.reset();
        assert_eq!(monitor.available_read(), 0);
        assert_eq!(monitor.underrun_count(), 0);

        let (mut producer, mut consumer, monitor) = stereo_ring(16);
        assert_eq!(producer.write_stereo(&[0.25, -0.25, 99.0]), 1);
        let mut short = [9.0];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut short, 1.0, false, 0, 0, 0),
            0
        );
        assert_eq!(short, [0.0]);
        assert_eq!(monitor.available_read(), 1);
        assert_eq!(monitor.underrun_count(), 0);
        assert_eq!(monitor.overrun_count(), 0);
    }

    #[test]
    fn priming_is_eighty_percent_without_false_underruns() {
        let (mut producer, mut consumer, monitor) = stereo_ring(64);
        assert_eq!(producer.write_stereo(&[0.25; 62]), 31);
        let mut out = [9.0; 9];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 40),
            0
        );
        assert_eq!(out, [0.0; 9]);
        assert_eq!(monitor.underrun_count(), 0);
        producer.write_stereo(&[0.25; 2]);
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 40),
            4
        );
        assert_eq!(out[0], 0.25);
        assert_eq!(out[8], 0.0);
        assert!(consumer.primed);
        assert_eq!(monitor.underrun_count(), 0);
    }

    #[test]
    fn startup_waits_for_a_complete_callback_and_starvation_reprimes_once() {
        let (mut producer, mut consumer, monitor) = stereo_ring(64);
        let mut out = [1.0; 32];
        producer.write_stereo(&[0.25; 32]);
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 16),
            0
        );
        assert_eq!(monitor.underrun_count(), 0);
        producer.write_stereo(&[0.25; 32]);
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 32),
            16
        );
        assert_eq!(monitor.available_read(), 16);
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 32),
            0
        );
        assert_eq!(out, [0.0; 32]);
        assert_eq!(monitor.underrun_count(), 1);
        for _ in 0..4 {
            assert_eq!(
                consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 32),
                0
            );
        }
        assert_eq!(monitor.underrun_count(), 1);
        producer.write_stereo(&[0.5; 32]);
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 32),
            16
        );
        assert_eq!(out[0], 0.25);
        assert_eq!(monitor.underrun_count(), 1);
    }

    #[test]
    fn newest_rejection_and_concurrent_full_recovery_preserve_stereo_order() {
        let (mut producer, mut consumer, monitor) = stereo_ring(32);
        let initial = ramp(32);
        assert_eq!(producer.write_stereo(&initial), 32);
        let (command_tx, command_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let new_audio = [0.75, -0.75].repeat(32);
            result_tx.send(producer.write_stereo(&new_audio)).unwrap();
            command_rx.recv().unwrap();
            result_tx.send(producer.write_stereo(&new_audio)).unwrap();
        });

        assert_eq!(result_rx.recv().unwrap(), 0);
        assert_eq!(monitor.available_read(), 32);
        let mut out = [0.0; 32];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 32),
            16
        );
        assert_filtered_sequence(&out, &initial, 0.0, 1.0, consumer.smoothed_ratio);
        command_tx.send(()).unwrap();
        assert_eq!(result_rx.recv().unwrap(), 16);
        writer.join().unwrap();
        assert_eq!(monitor.available_read(), 32);
        assert_eq!(monitor.overrun_count(), 2);
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 32),
            16
        );
        assert_filtered_sequence(&out, &initial, 16.0, 1.0, consumer.smoothed_ratio);
        let mut recovered = [0.0; 24];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut recovered, 1.0, false, 48_000, 48_000, 16),
            12
        );
        let input: Vec<f32> = initial
            .iter()
            .copied()
            .chain([0.75, -0.75].repeat(16))
            .collect();
        assert_filtered_sequence(&recovered, &input, 32.0, 1.0, consumer.smoothed_ratio);
        assert_eq!(monitor.underrun_count(), 0);
    }

    #[test]
    fn concurrent_streaming_never_tears_a_stereo_frame_or_loses_full_recovery() {
        use std::sync::atomic::AtomicBool;

        let (mut producer, mut consumer, monitor) = stereo_ring(127);
        let finished = Arc::new(AtomicBool::new(false));
        let writer_finished = Arc::clone(&finished);
        let writer = thread::spawn(move || {
            let block = [0.25, -0.25].repeat(31);
            let mut remaining = 20_000;
            while remaining != 0 {
                let frames = remaining.min(31);
                let written = producer.write_stereo(&block[..frames * 2]);
                remaining -= written;
                thread::yield_now();
            }
            writer_finished.store(true, Ordering::Release);
        });
        let mut output_frames = 0;
        let mut out = [0.0; 32];
        loop {
            let rendered = consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 64);
            if rendered == 0 {
                assert_eq!(out, [0.0; 32]);
                // The done acquire may observe newer writes than this callback did.
                if finished.load(Ordering::Acquire) && monitor.available_read() < 51 {
                    break;
                }
            } else {
                output_frames += rendered;
                for frame in out.chunks_exact(2) {
                    assert!((frame[0] + frame[1]).abs() < 1e-7);
                    assert!((frame[0] - 0.25).abs() < 1e-5);
                }
            }
            assert!(monitor.available_read() <= 127);
            thread::yield_now();
        }
        writer.join().unwrap();
        assert!(output_frames > 19_800 && output_frames < 20_200);
        assert!(monitor.available_read() < 52);
    }

    #[test]
    fn non_power_of_two_slots_remain_ordered_when_publication_counters_wrap() {
        let (mut producer, mut consumer, monitor) = stereo_ring(17);
        let start = u64::MAX - 10;
        producer.write_pos = start;
        producer.write_index = 5;
        consumer.read_pos = start;
        consumer.read_index = 5;
        monitor.shared.write_pos.store(start, Ordering::Release);
        monitor.shared.read_pos.store(start, Ordering::Release);
        let initial = ramp(17);
        assert_eq!(producer.write_stereo(&initial), 17);
        assert_eq!(monitor.available_read(), 17);
        let mut out = [0.0; 24];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 17),
            12
        );
        assert_filtered_sequence(&out, &initial, 0.0, 1.0, consumer.smoothed_ratio);
        assert_eq!(monitor.available_read(), 5);
        assert_eq!(producer.write_stereo(&[0.75, -0.75].repeat(12)), 12);
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 17),
            12
        );
        let input: Vec<f32> = initial
            .iter()
            .copied()
            .chain([0.75, -0.75].repeat(12))
            .collect();
        assert_filtered_sequence(&out, &input, 12.0, 1.0, consumer.smoothed_ratio);
        assert_eq!(monitor.available_read(), 5);
        assert_eq!(monitor.underrun_count(), 0);
        assert_eq!(monitor.overrun_count(), 0);
    }

    #[test]
    fn sinc_history_survives_immediate_reuse_of_freed_slots() {
        let (mut producer, mut consumer, monitor) = stereo_ring(16);
        let initial = ramp(16);
        producer.write_stereo(&initial);
        let mut first = [0.0; 8];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut first, 1.0, false, 48_000, 44_100, 16),
            4
        );
        let phase = consumer.resample_pos as f32;
        assert!(phase > 0.3 && phase < 0.4);
        assert_eq!(monitor.available_read(), 12);
        assert_eq!(producer.write_stereo(&[0.8; 8]), 4);
        let mut second = [0.0; 2];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut second, 1.0, false, 48_000, 44_100, 16),
            1
        );
        assert_filtered_sequence(
            &second,
            &initial,
            4.0 + f64::from(phase),
            48_000.0 / 44_100.0,
            consumer.smoothed_ratio,
        );
    }

    #[test]
    fn hard_snap_discards_only_from_the_consumer_after_eighty_five_ms_slack() {
        let (mut producer, mut consumer, monitor) = stereo_ring(8192);
        let input: Vec<f32> = (0..6000).flat_map(|i| [i as f32 / 10_000.0; 2]).collect();
        producer.write_stereo(&input);
        let mut out = [0.0; 64];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 480),
            32
        );
        assert!((out[0] - (5520 - RESAMPLER_DELAY_FRAMES) as f32 / 10_000.0).abs() < 1e-6);
        assert_eq!(monitor.available_read(), 448);
        assert_eq!(monitor.underrun_count(), 0);
        assert_eq!(monitor.overrun_count(), 0);
    }

    #[test]
    fn snap_keeps_a_whole_callback_even_with_a_too_small_latency_target() {
        let (mut producer, mut consumer, monitor) = stereo_ring(8192);
        producer.write_stereo(&[0.25; 12_000]);
        let mut out = [0.0; 128];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 0),
            64
        );
        assert!(out.iter().all(|sample| (*sample - 0.25).abs() < 1e-6));
        assert_eq!(monitor.underrun_count(), 0);
        assert!(monitor.available_read() >= 3);
    }

    #[test]
    fn rate_conversion_tracks_source_phase_and_zero_rates_use_default() {
        let (mut producer, mut consumer, monitor) = stereo_ring(2048);
        let input: Vec<f32> = (0..2000)
            .flat_map(|i| {
                let value =
                    (std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32 * 0.5;
                [value, -value]
            })
            .collect();
        producer.write_stereo(&input);
        let mut out = [0.0; 512];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 44_100, 2000),
            256
        );
        for (i, frame) in out.chunks_exact(2).enumerate() {
            let source_position = i as f64 * 48_000.0 / 44_100.0 - RESAMPLER_DELAY_FRAMES as f64;
            let expected =
                (std::f64::consts::TAU * 1000.0 * source_position / 48_000.0).sin() as f32 * 0.5;
            if source_position >= HISTORY_FRAMES as f64 - RESAMPLER_DELAY_FRAMES as f64 {
                assert!((frame[0] - expected).abs() < 0.001);
            }
            assert!((frame[0] + frame[1]).abs() < 1e-7);
        }
        assert_eq!(
            monitor.available_read(),
            2000 - (256.0_f64 * 48_000.0 / 44_100.0).floor() as usize
        );
        consumer.reset();
        producer.write_stereo(&[0.25; 64]);
        let mut out = [0.0; 16];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 0, 0, 32),
            8
        );
        assert_eq!(monitor.available_read(), 24);
    }

    #[test]
    fn drift_ema_and_ratio_slew_converge_within_eighteen_hundred_ppm() {
        for (fill, target, expected) in [(5000, 1000, 1.0018), (1000, 5000, 0.9982)] {
            let (mut producer, mut consumer, monitor) = stereo_ring(8192);
            producer.write_stereo(&vec![0.25; fill * 2]);
            // Low-fill case needs an already-primed stream before its fill drops.
            consumer.primed = true;
            consumer.smoothed_fill = fill as f64;
            consumer.smoothed_ratio = 1.0;
            let mut out = [0.0; 2];
            for iteration in 0..1000 {
                let before = monitor.available_read();
                assert_eq!(
                    consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, target),
                    1
                );
                if iteration == 0 {
                    assert!(
                        (consumer.smoothed_ratio - (1.0 * 0.92 + expected * 0.08)).abs() < 1e-12
                    );
                }
                let consumed = before - monitor.available_read();
                assert_eq!(producer.write_stereo(&[0.25; 4][..consumed * 2]), consumed);
                assert!(consumer.smoothed_ratio >= 0.9982 - 1e-12);
                assert!(consumer.smoothed_ratio <= 1.0018 + 1e-12);
            }
            assert!((consumer.smoothed_ratio - expected).abs() < 1e-12);
            assert_eq!(monitor.available_read(), fill);
            assert_eq!(monitor.underrun_count(), 0);
            let previous_fill = consumer.smoothed_fill;
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, target);
            producer.write_stereo(&[0.25; 22]);
            let observed_fill = monitor.available_read() as f64;
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, target);
            assert!(
                (consumer.smoothed_fill - (previous_fill * 0.96 + observed_fill * 0.04)).abs()
                    < 1e-9
            );
        }
    }

    #[test]
    fn mute_and_gain_changes_dezipper_and_reset_keeps_gain_state() {
        let (mut producer, mut consumer, monitor) = stereo_ring(512);
        producer.write_stereo(&[0.5; 800]);
        let mut out = [0.0; 400];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, true, 48_000, 48_000, 400),
            200
        );
        assert!((out[0] - 0.5 * (1.0 - consumer.gain_alpha)).abs() < 1e-7);
        assert!(
            (consumer.current_gain - (1.0 - f64::from(consumer.gain_alpha)).powi(200) as f32).abs()
                < 2e-6
        );
        assert!(
            out.chunks_exact(2)
                .zip(out[2..].chunks_exact(2))
                .all(|(previous, next)| next[0] < previous[0])
        );
        let gain = consumer.current_gain;
        consumer.reset();
        assert_eq!(monitor.available_read(), 0);
        assert_eq!(consumer.current_gain, gain);
        producer.write_stereo(&[0.5; 128]);
        let mut unmuted = [0.0; 64];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut unmuted, 1.0, false, 48_000, 48_000, 64),
            32
        );
        assert!(consumer.current_gain > gain && consumer.current_gain < 1.0);
        assert!(unmuted[0] > 0.5 * gain);
    }

    #[test]
    fn nonfinite_input_is_replaced_by_silence() {
        let (mut producer, mut consumer, _) = stereo_ring(16);
        producer.write_stereo(&[
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.25,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ]);
        let mut out = [1.0; 4];
        assert_eq!(
            consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 6),
            2
        );
        assert!(out.iter().all(|sample| sample.is_finite()));
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn gain_dezippering_has_equal_time_response_at_different_output_rates() {
        let expected = (-0.010 / GAIN_TIME_SECONDS).exp() as f32;
        for rate in [44_100, 48_000, 96_000, 384_000] {
            let (mut producer, mut consumer, _) = stereo_ring(8192);
            producer.write_stereo(&[0.5; 16_384]);
            let frames = rate as usize / 100;
            let mut out = vec![0.0; frames * 2];
            assert_eq!(
                consumer.read_stereo_adaptive(&mut out, 1.0, true, rate, rate, 8192),
                frames
            );
            assert!(
                (consumer.current_gain - expected).abs() < 2e-5,
                "rate={rate}"
            );
            assert_eq!(consumer.gain_rate, rate);
        }
    }

    #[test]
    fn tiny_ring_history_spans_many_reused_slots_without_allocating_in_drain() {
        let (mut producer, mut consumer, monitor) = stereo_ring(16);
        let input: Vec<f32> = (0..256)
            .flat_map(|index| {
                let value = (index as f32 * 0.17).sin() * 0.5;
                [value, -value]
            })
            .collect();
        assert_eq!(producer.write_stereo(&input[..32]), 16);
        let mut out = [0.0; 16];
        for block in 0..30 {
            assert_eq!(
                consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, 16),
                8
            );
            assert_filtered_sequence(&out, &input, (block * 8) as f64, 1.0, 1.0);
            let source = (16 + block * 8) * 2;
            assert_eq!(producer.write_stereo(&input[source..source + 16]), 8);
        }
        assert_eq!(monitor.underrun_count(), 0);
        assert_eq!(monitor.overrun_count(), 0);
    }

    #[test]
    fn smallest_renderable_ring_keeps_existing_four_frame_guard() {
        for capacity in 1..=5 {
            let (mut producer, mut consumer, _) = stereo_ring(capacity);
            producer.write_stereo(&vec![0.25; capacity * 2]);
            let mut out = [1.0; 2];
            let rendered =
                consumer.read_stereo_adaptive(&mut out, 1.0, false, 48_000, 48_000, capacity);
            if capacity < 5 {
                assert_eq!(rendered, 0);
                assert_eq!(out, [0.0; 2]);
            } else {
                assert_eq!(rendered, 1);
                assert_eq!(out, [0.25; 2]);
            }
        }
    }

    #[test]
    fn publication_cursors_occupy_distinct_cache_lines() {
        let (_, _, monitor) = stereo_ring(0);
        let writer = &monitor.shared.write_pos as *const PublishedCursor as usize;
        let reader = &monitor.shared.read_pos as *const PublishedCursor as usize;
        assert_eq!(writer % 64, 0);
        assert_eq!(reader % 64, 0);
        assert_ne!(writer / 64, reader / 64);
    }
}
