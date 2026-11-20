//! Dedicated 48kHz stereo loopback capture; this thread alone owns ring producers.
//! Bounded, recycled telemetry buffers keep analysis and publication off delivery.

use super::engine::WorkerLive;
use crate::dsp::{RingProducer, TelemetryAnalyzer, TelemetrySnapshot, soft_limit};
use crate::platform::{
    PriorityGuard,
    process::{AudioProcess, available, command},
};
use anyhow::{Context, Result};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) const SOURCE_RATE: u32 = 48_000;
const CHUNK_FRAMES: usize = 256;

const TELEMETRY_PERIOD: Duration = Duration::from_millis(16);
const TELEMETRY_IDLE: Duration = Duration::from_millis(100);
type StereoBlock = Box<[f32; CHUNK_FRAMES * 2]>;

/// Three owned buffers circulate without allocating or copying audio blocks.
/// If analysis falls behind, capture skips telemetry, never audio delivery.
struct TelemetryWorker {
    sender: Option<mpsc::SyncSender<StereoBlock>>,
    returned: mpsc::Receiver<StereoBlock>,
    spare: Option<StereoBlock>,
    worker: Option<JoinHandle<()>>,
}

impl TelemetryWorker {
    fn start(telemetry: Arc<Mutex<TelemetrySnapshot>>) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<StereoBlock>(2);
        let (recycle, returned) = mpsc::sync_channel(2);
        for _ in 0..2 {
            recycle.send(Box::new([0.0; CHUNK_FRAMES * 2]))?;
        }
        let worker = thread::Builder::new()
            .name("matrix-telemetry".into())
            .spawn(move || {
                let mut analyzer = TelemetryAnalyzer::new(SOURCE_RATE);
                let mut next_publish = Instant::now();
                loop {
                    let snapshot = match receiver.recv_timeout(TELEMETRY_IDLE) {
                        Ok(block) => {
                            let snapshot = analyzer.update(&block[..]);
                            // Capacity holds every buffer not currently owned by capture.
                            let _ = recycle.try_send(block);
                            if Instant::now() < next_publish {
                                continue;
                            }
                            snapshot
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => analyzer.decay(),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    *telemetry.lock().unwrap_or_else(|error| error.into_inner()) = snapshot;
                    next_publish = Instant::now() + TELEMETRY_PERIOD;
                }
                *telemetry.lock().unwrap_or_else(|error| error.into_inner()) = analyzer.decay();
            })
            .context("spawn telemetry thread")?;
        Ok(Self {
            sender: Some(sender),
            returned,
            spare: None,
            worker: Some(worker),
        })
    }

    fn submit(&mut self, stereo: &mut StereoBlock) {
        let replacement = self.spare.take().or_else(|| self.returned.try_recv().ok());
        if let Some(mut block) = replacement {
            std::mem::swap(stereo, &mut block);
            if let Err(error) = self.sender.as_ref().unwrap().try_send(block) {
                let mut block = match error {
                    mpsc::TrySendError::Full(block) | mpsc::TrySendError::Disconnected(block) => {
                        block
                    }
                };
                std::mem::swap(stereo, &mut block);
                self.spare = Some(block);
            }
        }
    }
}

impl Drop for TelemetryWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("Telemetry thread panicked");
        }
    }
}

pub(crate) enum CaptureCommand {
    Add(String, RingProducer),
    Remove(String),
}

pub(crate) struct CaptureHandle {
    pub sender: mpsc::Sender<CaptureCommand>,
    pub live: Arc<WorkerLive>,
    worker: Option<JoinHandle<()>>,
}

impl CaptureHandle {
    pub(crate) fn start(
        sink: String,
        live: Arc<WorkerLive>,
        telemetry: Arc<Mutex<TelemetrySnapshot>>,
    ) -> Result<Self> {
        // Spawn telemetry before raising capture priority: analysis stays background work.
        let telemetry = TelemetryWorker::start(telemetry)?;
        let (sender, receiver) = mpsc::channel();
        let (startup, ready) = mpsc::sync_channel(1);
        let worker_live = live.clone();
        let worker = thread::Builder::new()
            .name("matrix-capture".into())
            .spawn(move || {
                let priority = PriorityGuard::new();
                worker_live.nice.store(priority.nice, Ordering::Relaxed);
                worker_live
                    .realtime
                    .store(priority.realtime, Ordering::Relaxed);
                match spawn_capture(&sink) {
                    Ok(initial) => {
                        if startup.send(Ok(())).is_ok() {
                            capture_loop(sink, initial, receiver, worker_live, telemetry);
                        }
                    }
                    Err(error) => {
                        let _ = startup.send(Err(error));
                    }
                }
            })
            .context("spawn capture thread")?;
        let handle = Self {
            sender,
            live,
            worker: Some(worker),
        };
        ready
            .recv()
            .context("capture thread exited during startup")??;
        Ok(handle)
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.live.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("capture thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn spawn_capture(sink: &str) -> Result<AudioProcess> {
    let mut cmd;
    if available("parec") {
        cmd = command("parec");
        cmd.args([
            "--raw",
            &format!("--device={sink}.monitor"),
            "--format=float32le",
            "--rate=48000",
            "--channels=2",
            "--latency-msec=8",
            "--process-time-msec=4",
            "--client-name=pw_matrix_native_capture",
        ]);
    } else {
        cmd = command("pw-record");
        cmd.args(["--raw", "--target", sink, "--format", "f32", "--rate", "48000", "--channels", "2", "--latency=256", "--properties={ stream.capture.sink=true node.dont-reconnect=true media.role=DSP node.name=pw_matrix_native_capture }", "-"]);
    }
    AudioProcess::spawn(cmd, true)
}

fn apply_commands(
    receiver: &mpsc::Receiver<CaptureCommand>,
    targets: &mut Vec<(String, RingProducer)>,
) {
    for command in receiver.try_iter() {
        match command {
            CaptureCommand::Add(id, producer) => {
                if let Some(index) = targets.iter().position(|(key, _)| key == &id) {
                    targets.swap_remove(index);
                }
                targets.push((id, producer));
            }
            CaptureCommand::Remove(id) => {
                if let Some(index) = targets.iter().position(|(key, _)| key == &id) {
                    targets.swap_remove(index);
                }
            }
        }
    }
}

fn capture_loop(
    sink: String,
    initial: AudioProcess,
    receiver: mpsc::Receiver<CaptureCommand>,
    live: Arc<WorkerLive>,
    mut telemetry: TelemetryWorker,
) {
    let mut process = Some(initial);
    let mut targets = Vec::new();
    let mut raw = [0u8; CHUNK_FRAMES * 8];
    let mut stereo = Box::new([0.0f32; CHUNK_FRAMES * 2]);
    while !live.stop.load(Ordering::Acquire) {
        apply_commands(&receiver, &mut targets);
        if process.is_none() {
            match spawn_capture(&sink) {
                Ok(child) => process = Some(child),
                Err(error) => {
                    live.set_error(Some(format!("capture: {error:#}")));
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }
        let child = process.as_mut().unwrap();
        match child.read_block(&mut raw, &live.stop) {
            Ok(true) => {
                live.active.store(true, Ordering::Release);
                live.set_error(None);
                let gain = f32::from_bits(live.gain.load(Ordering::Relaxed));
                let muted = live.muted.load(Ordering::Relaxed);
                for (sample, bytes) in stereo.iter_mut().zip(raw.chunks_exact(4)) {
                    let input = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    *sample = if muted || !input.is_finite() {
                        0.0
                    } else {
                        soft_limit(input * gain)
                    };
                }
                apply_commands(&receiver, &mut targets);
                for (_, producer) in &mut targets {
                    producer.write_stereo(&stereo[..]);
                }
                telemetry.submit(&mut stereo);
            }
            Ok(false) => break,
            Err(error) => {
                let _ = child.terminate();
                live.active.store(false, Ordering::Release);
                live.set_error(Some(format!("capture: {error:#}; {}", child.error_text())));
                process.take();
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    live.active.store(false, Ordering::Release);
    drop(process);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturated_telemetry_never_replaces_or_mutates_current_audio() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let (recycle, returned) = mpsc::sync_channel(2);
        for _ in 0..2 {
            recycle.send(Box::new([0.0; CHUNK_FRAMES * 2])).unwrap();
        }
        let mut telemetry = TelemetryWorker {
            sender: Some(sender),
            returned,
            spare: None,
            worker: None,
        };
        let mut stereo = Box::new([0.0; CHUNK_FRAMES * 2]);
        stereo[0] = 0.5;
        stereo[1] = -0.25;
        telemetry.submit(&mut stereo);
        stereo[0] = 0.125;
        stereo[1] = 0.25;
        telemetry.submit(&mut stereo);

        stereo[0] = -0.75;
        stereo[1] = 0.625;
        let pointer = stereo.as_ptr();
        for _ in 0..100 {
            telemetry.submit(&mut stereo);
        }
        assert_eq!(stereo.as_ptr(), pointer);
        assert_eq!(&stereo[..2], &[-0.75, 0.625]);
        assert_eq!(&receiver.try_recv().unwrap()[..2], &[0.5, -0.25]);
        assert_eq!(&receiver.try_recv().unwrap()[..2], &[0.125, 0.25]);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn telemetry_shutdown_drains_coherent_stereo_metrics_and_stateful_holds() {
        let snapshot = Arc::new(Mutex::new(TelemetrySnapshot::default()));
        let worker = TelemetryWorker::start(snapshot.clone()).unwrap();
        let mut input = Box::new([0.0; CHUNK_FRAMES * 2]);
        for frame in input.chunks_exact_mut(2) {
            frame[0] = 0.5;
            frame[1] = -0.25;
        }
        let quiet = Box::new([0.0; CHUNK_FRAMES * 2]);
        let mut analyzer = TelemetryAnalyzer::new(SOURCE_RATE);
        analyzer.update(&input[..]);
        analyzer.update(&quiet[..]);
        let expected = analyzer.decay();
        worker.sender.as_ref().unwrap().send(input).unwrap();
        worker.sender.as_ref().unwrap().send(quiet).unwrap();
        drop(worker);
        assert_eq!(*snapshot.lock().unwrap(), expected);
    }
}
