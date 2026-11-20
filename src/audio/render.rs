//! Dedicated endpoint playback; this thread alone owns its ring consumer.

use super::{capture::SOURCE_RATE, engine::WorkerLive};
use crate::dsp::RingConsumer;
use crate::platform::{
    PriorityGuard,
    process::{AudioProcess, available, command},
};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const CHUNK_FRAMES: usize = 256;

pub(crate) struct RenderHandle {
    pub live: Arc<WorkerLive>,
    pub rate: u32,
    worker: Option<JoinHandle<()>>,
}

impl RenderHandle {
    pub(crate) fn start(
        id: String,
        rate: u32,
        mut consumer: RingConsumer,
        live: Arc<WorkerLive>,
    ) -> Result<Self> {
        consumer.prepare_rates(SOURCE_RATE, rate);
        let (startup, ready) = std::sync::mpsc::sync_channel(1);
        let worker_live = live.clone();
        let worker = thread::Builder::new()
            .name("matrix-render".into())
            .spawn(move || {
                let priority = PriorityGuard::new();
                worker_live.nice.store(priority.nice, Ordering::Relaxed);
                worker_live
                    .realtime
                    .store(priority.realtime, Ordering::Relaxed);
                match spawn_render(&id, rate) {
                    Ok(initial) => {
                        if startup.send(Ok(())).is_ok() {
                            render_loop(id, rate, initial, consumer, worker_live);
                        }
                    }
                    Err(error) => {
                        let _ = startup.send(Err(error));
                    }
                }
            })
            .context("spawn playback thread")?;
        let handle = Self {
            live,
            rate,
            worker: Some(worker),
        };
        ready
            .recv()
            .context("playback thread exited during startup")??;
        Ok(handle)
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.live.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("playback thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for RenderHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn spawn_render(id: &str, rate: u32) -> Result<AudioProcess> {
    let mut cmd;
    if available("pacat") {
        cmd = command("pacat");
        cmd.args([
            "--raw",
            "--playback",
            &format!("--device={id}"),
            "--client-name=pw_matrix_native_router",
            "--stream-name=pw_matrix_native_out",
            "--format=float32le",
            &format!("--rate={rate}"),
            "--channels=2",
            "--latency-msec=12",
            "--process-time-msec=4",
        ]);
    } else {
        cmd = command("pw-play");
        cmd.args(["--raw", "--target", id, "--format", "f32", "--rate", &rate.to_string(), "--channels", "2", "--latency=12ms", "--properties={ node.dont-reconnect=true media.role=DSP node.name=pw_matrix_native_out application.name=pw_matrix_native_router }", "-"]);
    }
    AudioProcess::spawn(cmd, false)
}

fn render_loop(
    id: String,
    rate: u32,
    initial: AudioProcess,
    mut consumer: RingConsumer,
    live: Arc<WorkerLive>,
) {
    let mut process = Some(initial);
    let mut stereo = [0.0f32; CHUNK_FRAMES * 2];
    let mut bytes = [0u8; CHUNK_FRAMES * 8];
    consumer.reset();
    while !live.stop.load(Ordering::Acquire) {
        if process.is_none() {
            match spawn_render(&id, rate) {
                Ok(child) => {
                    process = Some(child);
                    consumer.reset();
                }
                Err(error) => {
                    live.set_error(Some(format!("playback: {error:#}")));
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }
        let gain = f32::from_bits(live.gain.load(Ordering::Relaxed));
        let muted = live.muted.load(Ordering::Relaxed);
        consumer.read_stereo_adaptive(
            &mut stereo,
            gain,
            muted,
            SOURCE_RATE,
            rate,
            live.latency_frames.load(Ordering::Relaxed),
        );
        for (sample, output) in stereo.iter().zip(bytes.chunks_exact_mut(4)) {
            output.copy_from_slice(&sample.to_le_bytes());
        }
        let child = process.as_mut().unwrap();
        match child.write_block(&bytes, &live.stop) {
            Ok(true) => {
                live.active.store(true, Ordering::Release);
                live.set_error(None);
            }
            Ok(false) => break,
            Err(error) => {
                let _ = child.terminate();
                live.active.store(false, Ordering::Release);
                live.set_error(Some(format!("playback: {error:#}; {}", child.error_text())));
                process.take();
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    live.active.store(false, Ordering::Release);
    drop(process);
}
