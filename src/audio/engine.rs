//! Plain-thread Linux audio routing service. All OS/config I/O lives on the control plane.

use super::{
    capture::{CaptureCommand, CaptureHandle, SOURCE_RATE},
    device::{DeviceInfo, query_sinks},
    render::RenderHandle,
};
use crate::config::{self, EndpointSetting, PersistentConfig, clamp_volume};
use crate::dsp::{RingMonitor, TelemetrySnapshot, split_hybrid_volume, stereo_ring};
use crate::platform::process::pactl;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const USER_GRACE: Duration = Duration::from_millis(650);
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);
const POLL_PERIOD: Duration = Duration::from_millis(500);
const DEFAULT_HUB: &str = "pw_matrix_virtual_sink";
const MIN_RING_MS: f64 = 8.0;
const FILTER_DELAY_MS: f64 =
    crate::dsp::ring_buffer::RESAMPLER_DELAY_FRAMES as f64 * 1000.0 / SOURCE_RATE as f64;

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct EngineOptions {
    pub config_path: Option<PathBuf>,
    pub virtual_sink_name: String,
    pub manage_default: bool,
    pub move_existing_streams: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            config_path: None,
            virtual_sink_name: DEFAULT_HUB.into(),
            manage_default: true,
            move_existing_streams: true,
        }
    }
}

#[derive(Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct EndpointSnapshot {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub active: bool,
    pub desired_active: bool,
    pub muted: bool,
    pub volume: f32,
    pub offset_ms: i32,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits: u32,
    /// Observed unread source-frame occupancy, not the requested ring target.
    pub ring_ms: f64,
    /// Requested playback-buffer estimate, not measured hardware or server latency.
    pub hardware_ms: f64,
    /// Partial software estimate: ring occupancy, filter delay, and requested playback buffer.
    /// Excludes capture buffering, OS pipes, server scheduling, and actual DAC latency.
    pub latency_ms: f64,
    pub underruns: u64,
    pub overruns: u64,
    pub error: Option<String>,
}

#[derive(Clone, Default)]
#[non_exhaustive]
pub struct EngineSnapshot {
    pub endpoints: Vec<EndpointSnapshot>,
    pub master_volume: f32,
    pub master_muted: bool,
    /// Requested ring floor; workers retain an effective minimum of 8 ms.
    pub target_floor_ms: u32,
    pub auto_sync: bool,
    pub source_rate: u32,
    pub capturing: bool,
    pub running: bool,
    pub startup_failed: bool,
    pub status: String,
    pub error: Option<String>,
    pub telemetry: TelemetrySnapshot,
}

pub(crate) struct WorkerLive {
    pub stop: AtomicBool,
    pub active: AtomicBool,
    pub gain: AtomicU32,
    pub muted: AtomicBool,
    pub latency_frames: AtomicUsize,
    pub nice: AtomicI32,
    pub realtime: AtomicBool,
    error_present: AtomicBool,
    error: Mutex<Option<String>>,
}

impl WorkerLive {
    fn new(volume: f32, muted: bool) -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            active: AtomicBool::new(false),
            gain: AtomicU32::new(split_hybrid_volume(volume).1.to_bits()),
            muted: AtomicBool::new(muted),
            latency_frames: AtomicUsize::new(768),
            nice: AtomicI32::new(0),
            realtime: AtomicBool::new(false),
            error_present: AtomicBool::new(false),
            error: Mutex::new(None),
        })
    }

    fn update_gain(&self, volume: f32, muted: bool) {
        self.gain
            .store(split_hybrid_volume(volume).1.to_bits(), Ordering::Relaxed);
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub(crate) fn set_error(&self, error: Option<String>) {
        if error.is_none() && !self.error_present.load(Ordering::Relaxed) {
            return;
        }
        self.error_present.store(error.is_some(), Ordering::Relaxed);
        *self.error.lock().unwrap_or_else(|error| error.into_inner()) = error;
    }

    fn error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

#[derive(Clone, Copy)]
struct WriteValue<T> {
    value: T,
    revision: u64,
}

#[derive(Clone, Default)]
struct VolumeWrite {
    volume: Option<WriteValue<f32>>,
    muted: Option<WriteValue<bool>>,
}

#[derive(Default)]
struct FeedbackGuard {
    volume_at: Option<Instant>,
    mute_at: Option<Instant>,
}

struct EndpointLive {
    worker: Arc<WorkerLive>,
    monitor: RingMonitor,
}

#[derive(Default)]
struct Touched {
    master_volume: bool,
    master_muted: bool,
    floor: bool,
    auto_sync: bool,
    endpoints: HashMap<String, u8>,
}

struct State {
    config: PersistentConfig,
    config_loaded: bool,
    touched: Touched,
    devices: HashMap<String, DeviceInfo>,
    live: HashMap<String, EndpointLive>,
    guards: HashMap<String, FeedbackGuard>,
    writes: BTreeMap<String, VolumeWrite>,
    write_errors: BTreeMap<String, String>,
    control_errors: BTreeMap<&'static str, String>,
    routing_revision: u64,
    revision: u64,
    saved_revision: u64,
    dirty_at: Option<Instant>,
    status: String,
    error: Option<String>,
    running: bool,
    startup_failed: bool,
    refresh: Vec<mpsc::SyncSender<std::result::Result<(), String>>>,
}

impl State {
    fn dirty(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.dirty_at = Some(Instant::now());
    }

    fn queue_volume(&mut self, id: &str, volume: Option<f32>, muted: Option<bool>) {
        let revision = self.revision;
        let write = self.writes.entry(id.to_owned()).or_default();
        let guard = self.guards.entry(id.to_owned()).or_default();
        if let Some(volume) = volume {
            write.volume = Some(WriteValue {
                value: split_hybrid_volume(volume).0,
                revision,
            });
            guard.volume_at = Some(Instant::now());
        }
        if let Some(muted) = muted {
            write.muted = Some(WriteValue {
                value: muted,
                revision,
            });
            guard.mute_at = Some(Instant::now());
        }
    }

    fn recompute_latencies(&self) {
        let floor = (self.config.target_floor_ms as f64).max(MIN_RING_MS);
        // Source rate and FIR history delay are identical for all endpoints, so
        // that common delay cancels in relative alignment. Absolute estimates
        // include FILTER_DELAY_MS in snapshot(). Hardware values remain requested
        // playback-buffer estimates, not measurements or DAC clock phase locks.
        let max_total = self
            .devices
            .iter()
            .filter_map(|(id, device)| {
                let setting = self.config.endpoints.get(id)?;
                setting
                    .active
                    .then_some(floor + device.hardware_ms + setting.extra_offset_ms as f64)
            })
            .fold(0.0f64, f64::max);
        for (id, live) in &self.live {
            if let (Some(device), Some(setting)) =
                (self.devices.get(id), self.config.endpoints.get(id))
            {
                let minimum = floor + setting.extra_offset_ms as f64;
                let ring_ms = if self.config.auto_sync_mode {
                    minimum.max(max_total - device.hardware_ms)
                } else {
                    minimum
                };
                live.worker.latency_frames.store(
                    (ring_ms.clamp(MIN_RING_MS, 1500.0) * SOURCE_RATE as f64 / 1000.0) as usize,
                    Ordering::Relaxed,
                );
            }
        }
    }
}

#[derive(Default)]
struct Lifecycle {
    worker: Option<JoinHandle<std::result::Result<(), String>>>,
    shutdown_error: Option<String>,
    shutdown_done: bool,
}

pub struct AudioEngine {
    options: EngineOptions,
    state: Mutex<State>,
    wake: Condvar,
    lifecycle: Mutex<Lifecycle>,
    stopping: AtomicBool,
    master: Arc<WorkerLive>,
    telemetry: Arc<Mutex<TelemetrySnapshot>>,
}

impl AudioEngine {
    pub fn new() -> Arc<Self> {
        Self::with_options(EngineOptions::default())
    }

    pub fn with_options(options: EngineOptions) -> Arc<Self> {
        Arc::new(Self {
            options,
            state: Mutex::new(State {
                config: PersistentConfig::default(),
                config_loaded: false,
                touched: Touched::default(),
                devices: HashMap::new(),
                live: HashMap::new(),
                guards: HashMap::new(),
                writes: BTreeMap::new(),
                write_errors: BTreeMap::new(),
                control_errors: BTreeMap::new(),
                routing_revision: 0,
                revision: 0,
                saved_revision: 0,
                dirty_at: None,
                status: "Not started".into(),
                error: None,
                running: false,
                startup_failed: false,
                refresh: Vec::new(),
            }),
            wake: Condvar::new(),
            lifecycle: Mutex::new(Lifecycle::default()),
            stopping: AtomicBool::new(false),
            master: WorkerLive::new(1.0, false),
            telemetry: Arc::new(Mutex::new(TelemetrySnapshot::default())),
        })
    }

    pub fn start(self: &Arc<Self>) -> Result<()> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.stopping.load(Ordering::Acquire) {
            bail!("engine is shutting down or stopped");
        }
        if lifecycle.worker.is_some() {
            return Ok(());
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.status = "Starting audio service…".into();
            state.error = None;
            state.startup_failed = false;
            state.control_errors.clear();
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let engine = self.clone();
        let worker = match thread::Builder::new()
            .name("matrix-control".into())
            .spawn(move || engine.control_thread(sender))
        {
            Ok(worker) => worker,
            Err(error) => {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.startup_failed = true;
                state.status = "Audio startup failed".into();
                state.error = Some(format!("spawn audio control thread: {error}"));
                return Err(error).context("spawn audio control thread");
            }
        };
        lifecycle.worker = Some(worker);
        let result = receiver
            .recv()
            .unwrap_or_else(|_| Err("audio control thread exited during startup".into()));
        if let Err(error) = result {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.startup_failed = true;
            state.status = "Audio startup failed".into();
            state.error = Some(error.clone());
            drop(state);
            if let Some(worker) = lifecycle.worker.take() {
                let _ = worker.join();
            }
            bail!("{error}");
        }
        Ok(())
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut endpoints = Vec::with_capacity(state.devices.len());
        let mut pending = 0;
        for (id, device) in &state.devices {
            let Some(setting) = state.config.endpoints.get(id) else {
                continue;
            };
            let live = state.live.get(id);
            let active = setting.active
                && live.is_some_and(|live| live.worker.active.load(Ordering::Acquire));
            if setting.active && !active {
                pending += 1;
            }
            let ring_ms = live.map_or(0.0, |live| {
                live.monitor.available_read() as f64 * 1000.0 / SOURCE_RATE as f64
            });
            let filter_ms = live.map_or(0.0, |_| FILTER_DELAY_MS);
            endpoints.push(EndpointSnapshot {
                id: id.clone(),
                name: device.name.clone(),
                is_default: device.is_default,
                active,
                desired_active: setting.active,
                muted: setting.muted,
                volume: setting.volume,
                offset_ms: setting.extra_offset_ms,
                sample_rate: device.sample_rate,
                channels: device.channels,
                bits: device.bits,
                ring_ms,
                hardware_ms: device.hardware_ms,
                latency_ms: ring_ms + filter_ms + device.hardware_ms,
                underruns: live.map_or(0, |live| live.monitor.underrun_count()),
                overruns: live.map_or(0, |live| live.monitor.overrun_count()),
                error: live
                    .and_then(|live| live.worker.error())
                    .or_else(|| state.write_errors.get(id).cloned()),
            });
        }
        endpoints.sort_unstable_by(|left, right| {
            right
                .is_default
                .cmp(&left.is_default)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.id.cmp(&right.id))
        });
        let capturing = state.running && self.master.active.load(Ordering::Acquire);
        let capture_error = self.master.error();
        let status = if state.running {
            if !capturing {
                "Waiting for virtual monitor capture…".to_owned()
            } else if pending > 0 {
                format!("Routing audio · {pending} output(s) pending")
            } else {
                format!(
                    "Routing audio · {} active output(s)",
                    endpoints.iter().filter(|endpoint| endpoint.active).count()
                )
            }
        } else {
            state.status.clone()
        };
        let mut snapshot = EngineSnapshot {
            endpoints,
            master_volume: state.config.master_volume,
            master_muted: state.config.master_muted,
            target_floor_ms: state.config.target_floor_ms,
            auto_sync: state.config.auto_sync_mode,
            source_rate: SOURCE_RATE,
            capturing,
            running: state.running,
            startup_failed: state.startup_failed,
            status,
            error: state
                .error
                .clone()
                .or(capture_error)
                .or_else(|| state.control_errors.values().next().cloned())
                .or_else(|| state.write_errors.values().next().cloned()),
            telemetry: TelemetrySnapshot::default(),
        };
        drop(state);
        snapshot.telemetry = *self
            .telemetry
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        snapshot
    }

    pub fn set_master_volume(&self, volume: f32) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        state.config.master_volume = clamp_volume(volume);
        state.touched.master_volume = true;
        state.dirty();
        let volume = state.config.master_volume;
        let muted = state.config.master_muted;
        state.queue_volume(&self.options.virtual_sink_name, Some(volume), None);
        self.master.update_gain(volume, muted);
        self.wake.notify_one();
    }

    pub fn toggle_master_mute(&self) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        state.config.master_muted = !state.config.master_muted;
        state.touched.master_muted = true;
        state.dirty();
        let muted = state.config.master_muted;
        let volume = state.config.master_volume;
        state.queue_volume(&self.options.virtual_sink_name, None, Some(muted));
        self.master.update_gain(volume, muted);
        self.wake.notify_one();
    }

    fn edit_endpoint(&self, id: &str, mask: u8, edit: impl FnOnce(&mut EndpointSetting)) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        if !state.devices.contains_key(id) && !state.config.endpoints.contains_key(id) {
            state.error = Some(format!("Output {id} is unavailable; refresh devices first"));
            return;
        }
        let setting = state.config.endpoints.entry(id.to_owned()).or_default();
        edit(setting);
        let (volume, muted) = (setting.volume, setting.muted);
        *state.touched.endpoints.entry(id.to_owned()).or_default() |= mask;
        state.dirty();
        if mask & 1 != 0 {
            state.routing_revision = state.routing_revision.wrapping_add(1);
        }
        if mask & 2 != 0 {
            state.queue_volume(id, Some(volume), None);
        }
        if mask & 4 != 0 {
            state.queue_volume(id, None, Some(muted));
        }
        if let Some(live) = state.live.get(id) {
            live.worker.update_gain(volume, muted);
        }
        state.recompute_latencies();
        self.wake.notify_one();
    }

    pub fn set_device_volume(&self, id: &str, volume: f32) {
        self.edit_endpoint(id, 2, |setting| setting.volume = clamp_volume(volume));
    }
    pub fn toggle_device_mute(&self, id: &str) {
        self.edit_endpoint(id, 4, |setting| setting.muted = !setting.muted);
    }
    pub fn toggle_target(self: &Arc<Self>, id: &str) {
        self.edit_endpoint(id, 1, |setting| setting.active = !setting.active);
    }
    pub fn set_device_offset(&self, id: &str, ms: i32) {
        self.edit_endpoint(id, 8, |setting| setting.extra_offset_ms = ms.clamp(0, 1000));
    }

    pub fn set_target_floor(&self, ms: u32) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        state.config.target_floor_ms = ms.clamp(6, 100);
        state.touched.floor = true;
        state.dirty();
        state.recompute_latencies();
        self.wake.notify_one();
    }

    pub fn set_auto_sync(&self, enabled: bool) {
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        state.config.auto_sync_mode = enabled;
        state.touched.auto_sync = true;
        state.dirty();
        state.recompute_latencies();
        self.wake.notify_one();
    }

    pub fn refresh_devices(self: &Arc<Self>) -> Result<()> {
        self.start()?;
        let (sender, receiver) = mpsc::sync_channel(1);
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if self.stopping.load(Ordering::Acquire) || !state.running {
                bail!("engine is not running");
            }
            state.refresh.push(sender);
            self.wake.notify_one();
        }
        match receiver.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => bail!("{error}"),
            Err(_) => bail!("audio service stopped during refresh"),
        }
    }

    pub fn shutdown(&self) -> Result<()> {
        // Set cancellation before taking the lifecycle lock: this interrupts an in-flight start.
        self.stopping.store(true, Ordering::Release);
        self.master.stop.store(true, Ordering::Release);
        self.wake.notify_all();
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !lifecycle.shutdown_done {
            let result = if let Some(worker) = lifecycle.worker.take() {
                worker
                    .join()
                    .unwrap_or_else(|_| Err("audio control thread panicked".into()))
            } else {
                self.flush_config().map_err(|error| format!("{error:#}"))
            };
            lifecycle.shutdown_error = result.err();
            lifecycle.shutdown_done = true;
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.running = false;
            state.status = "Stopped".into();
            if let Some(error) = &lifecycle.shutdown_error {
                state.error = Some(error.clone());
            }
            for reply in state.refresh.drain(..) {
                let _ = reply.send(Err("engine stopped".into()));
            }
        }
        if let Some(error) = &lifecycle.shutdown_error {
            bail!("{error}");
        }
        Ok(())
    }

    fn config_path(&self) -> PathBuf {
        self.options
            .config_path
            .clone()
            .unwrap_or_else(config::default_path)
    }

    fn load_config(&self) -> Result<()> {
        if self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .config_loaded
        {
            return Ok(());
        }
        let (mut loaded, imported) =
            config::load(&self.config_path(), self.options.config_path.is_none())?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.touched.master_volume {
            loaded.master_volume = state.config.master_volume;
        }
        if state.touched.master_muted {
            loaded.master_muted = state.config.master_muted;
        }
        if state.touched.floor {
            loaded.target_floor_ms = state.config.target_floor_ms;
        }
        if state.touched.auto_sync {
            loaded.auto_sync_mode = state.config.auto_sync_mode;
        }
        for (id, mask) in &state.touched.endpoints {
            if let Some(current) = state.config.endpoints.get(id) {
                let setting = loaded.endpoints.entry(id.clone()).or_default();
                if mask & 1 != 0 {
                    setting.active = current.active;
                }
                if mask & 2 != 0 {
                    setting.volume = current.volume;
                }
                if mask & 4 != 0 {
                    setting.muted = current.muted;
                }
                if mask & 8 != 0 {
                    setting.extra_offset_ms = current.extra_offset_ms;
                }
            }
        }
        state.config = loaded;
        state.config_loaded = true;
        if imported {
            state.dirty();
        }
        self.master
            .update_gain(state.config.master_volume, state.config.master_muted);
        Ok(())
    }

    fn flush_config(&self) -> Result<()> {
        let (snapshot, revision) = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.revision == state.saved_revision {
                return Ok(());
            }
            // Do not overwrite a malformed existing file after failed startup.
            if !state.config_loaded {
                drop(state);
                self.load_config()?;
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                (state.config.clone(), state.revision)
            } else {
                (state.config.clone(), state.revision)
            }
        };
        config::save(&self.config_path(), &snapshot)?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.saved_revision = revision;
        if state.revision == revision {
            state.dirty_at = None;
        }
        Ok(())
    }

    fn record_error(&self, error: &anyhow::Error) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .error = Some(format!("{error:#}"));
    }

    fn set_control_error(&self, key: &'static str, error: Option<&anyhow::Error>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(error) = error {
            state.control_errors.insert(key, format!("{error:#}"));
        } else {
            state.control_errors.remove(key);
        }
    }

    fn control_thread(
        self: Arc<Self>,
        startup: mpsc::SyncSender<std::result::Result<(), String>>,
    ) -> std::result::Result<(), String> {
        let mut runtime = Runtime::default();
        let startup_result = self.initialize(&mut runtime);
        if let Err(error) = &startup_result {
            self.record_error(error);
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .status = "Audio startup failed".into();
        }
        let started = startup_result.is_ok();
        if started {
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .running = true;
            let _ = startup.send(Ok(()));
        }
        if started {
            let mut next_poll = Instant::now() + POLL_PERIOD;
            let mut last_write = String::new();
            while !self.stopping.load(Ordering::Acquire) {
                let refresh = {
                    let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                    std::mem::take(&mut state.refresh)
                };
                if Instant::now() >= next_poll || !refresh.is_empty() {
                    let result = self.poll_devices(&runtime).and_then(|_| {
                        if !refresh.is_empty() && self.options.move_existing_streams {
                            self.move_streams()?;
                        }
                        Ok(())
                    });
                    self.set_control_error("poll", result.as_ref().err());
                    for reply in refresh {
                        let _ = reply.send(
                            result
                                .as_ref()
                                .map(|_| ())
                                .map_err(|error| format!("{error:#}")),
                        );
                    }
                    next_poll = Instant::now() + POLL_PERIOD;
                }
                let reconcile = self.reconcile_targets(&mut runtime);
                self.set_control_error("outputs", reconcile.as_ref().err());
                self.apply_one_write(&mut last_write, false);
                let save_due = self
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .dirty_at
                    .is_some_and(|at| at.elapsed() >= SAVE_DEBOUNCE);
                if save_due {
                    let saved = self.flush_config();
                    self.set_control_error("config", saved.as_ref().err());
                    if saved.is_err() {
                        self.state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .dirty_at = Some(Instant::now());
                    }
                }
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                drop(self.wake.wait_timeout(state, Duration::from_millis(40)));
            }
        }
        let cleanup = self.cleanup(&mut runtime);
        if !started {
            let mut error = startup_result.unwrap_err();
            if let Err(cleanup) = cleanup {
                error = error.context(format!("cleanup also failed: {cleanup:#}"));
            }
            self.record_error(&error);
            let message = format!("{error:#}");
            let _ = startup.send(Err(message.clone()));
            return Err(message);
        }
        cleanup.map_err(|error| format!("{error:#}"))
    }

    fn initialize(&self, runtime: &mut Runtime) -> Result<()> {
        let name = &self.options.virtual_sink_name;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
        {
            bail!("virtual sink name must contain only letters, digits, underscore, dash or dot");
        }
        self.load_config()?;
        runtime.lock = Some(lock_sink(name)?);
        runtime.previous_default = pactl_text(&["get-default-sink"], Some(&self.stopping))?;
        let sinks = query_sinks(&runtime.previous_default, &self.stopping)?;
        if sinks.iter().any(|sink| sink.info.id == *name) {
            bail!(
                "Sink '{name}' already exists. Another router may be running; stop that owner or use a unique EngineOptions.virtual_sink_name. No existing sink or module was changed."
            );
        }
        let token = format!("{}-{}", std::process::id(), owner_serial());
        runtime.owner_token = token.clone();
        let properties = format!(
            "sink_properties=device.description=\"PW-Matrix Native Master Hub\" pw_matrix.owner={token}"
        );
        let load = pactl_text(
            &[
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
                "rate=48000",
                "channels=2",
                "format=float32le",
                &properties,
            ],
            Some(&self.stopping),
        );
        match load {
            Ok(id) => {
                if id.parse::<u32>().is_err() {
                    if let Ok(modules) = pactl_text(&["list", "short", "modules"], None) {
                        runtime.module_id = owned_module(&modules, name, &token);
                    }
                    bail!("pactl returned invalid module id: {id}");
                }
                runtime.module_id = Some(id);
            }
            Err(error) => {
                // A timed-out reply can still have created the module. Recover only OUR token.
                if let Ok(modules) = pactl_text(&["list", "short", "modules"], None) {
                    runtime.module_id = owned_module(&modules, name, &token);
                }
                return Err(error).context("create virtual master sink");
            }
        }
        pactl(&["suspend-sink", name, "0"], Some(&self.stopping))?;
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let (volume, muted) = (state.config.master_volume, state.config.master_muted);
            state.queue_volume(name, Some(volume), Some(muted));
        }
        let mut cursor = String::new();
        self.apply_one_write(&mut cursor, false);
        if self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .writes
            .contains_key(name)
        {
            bail!("initial master volume/mute write was not acknowledged");
        }
        if self.options.manage_default {
            runtime.default_changed = true;
            pactl(&["set-default-sink", name], Some(&self.stopping))?;
        }
        if self.options.move_existing_streams {
            self.move_streams()?;
        }
        self.master.stop.store(false, Ordering::Release);
        runtime.capture = Some(CaptureHandle::start(
            name.clone(),
            self.master.clone(),
            self.telemetry.clone(),
        )?);
        self.poll_devices(runtime)?;
        self.reconcile_targets(runtime)?;
        if self.stopping.load(Ordering::Acquire) {
            bail!("startup cancelled by shutdown");
        }
        Ok(())
    }

    fn poll_devices(&self, runtime: &Runtime) -> Result<()> {
        let current_default = pactl_text(&["get-default-sink"], Some(&self.stopping))?;
        let display_default = if current_default == self.options.virtual_sink_name {
            &runtime.previous_default
        } else {
            &current_default
        };
        let sinks = query_sinks(display_default, &self.stopping)?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut changed = false;
        let allow_initial_default =
            self.options.manage_default && state.config.endpoints.is_empty();
        let mut devices = HashMap::new();
        for sink in sinks {
            let id = &sink.info.id;
            if id == &self.options.virtual_sink_name {
                let pending = state.writes.get(id).cloned().unwrap_or_default();
                let guard = state.guards.entry(id.clone()).or_default();
                let allow_volume = feedback_allowed(guard.volume_at, pending.volume.is_some());
                let allow_mute = feedback_allowed(guard.mute_at, pending.muted.is_some());
                if allow_volume {
                    let volume = feedback_volume(state.config.master_volume, sink.volume);
                    if (volume - state.config.master_volume).abs() > 0.0001 {
                        state.config.master_volume = volume;
                        changed = true;
                    }
                }
                if allow_mute && state.config.master_muted != sink.muted {
                    state.config.master_muted = sink.muted;
                    changed = true;
                }
                self.master
                    .update_gain(state.config.master_volume, state.config.master_muted);
                continue;
            }
            // Other routers' hubs are never offered as output targets (no feedback loops).
            if id == DEFAULT_HUB || sink.router_sink {
                continue;
            }
            let newly_connected = !state.devices.contains_key(id);
            let has_saved = state.config.endpoints.contains_key(id);
            if !has_saved {
                state.config.endpoints.insert(
                    id.clone(),
                    EndpointSetting {
                        name: sink.info.name.clone(),
                        volume: sink.volume,
                        muted: sink.muted,
                        active: allow_initial_default && sink.info.is_default && !sink.virtual_sink,
                        extra_offset_ms: 0,
                    },
                );
                changed = true;
            }
            if newly_connected && has_saved {
                let setting = state.config.endpoints[id].clone();
                state.queue_volume(id, Some(setting.volume), Some(setting.muted));
            } else {
                let pending = state.writes.get(id).cloned().unwrap_or_default();
                let guard = state.guards.entry(id.clone()).or_default();
                let allow_volume = feedback_allowed(guard.volume_at, pending.volume.is_some());
                let allow_mute = feedback_allowed(guard.mute_at, pending.muted.is_some());
                let setting = state.config.endpoints.get_mut(id).unwrap();
                if allow_volume {
                    let volume = feedback_volume(setting.volume, sink.volume);
                    if (volume - setting.volume).abs() > 0.0001 {
                        setting.volume = volume;
                        changed = true;
                    }
                }
                if allow_mute && setting.muted != sink.muted {
                    setting.muted = sink.muted;
                    changed = true;
                }
            }
            let setting = state.config.endpoints.get_mut(id).unwrap();
            if setting.name != sink.info.name {
                setting.name = sink.info.name.clone();
                changed = true;
            }
            if let Some(live) = state.live.get(id) {
                let setting = &state.config.endpoints[id];
                live.worker.update_gain(setting.volume, setting.muted);
            }
            devices.insert(id.clone(), sink.info);
        }
        if devices.len() != state.devices.len()
            || devices.iter().any(|(id, device)| {
                state
                    .devices
                    .get(id)
                    .is_none_or(|old| old.sample_rate != device.sample_rate)
            })
        {
            state.routing_revision = state.routing_revision.wrapping_add(1);
        }
        state.devices = devices;
        state.recompute_latencies();
        if changed {
            state.dirty();
        }
        Ok(())
    }

    fn reconcile_targets(&self, runtime: &mut Runtime) -> Result<()> {
        let wanted = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let same_revision = runtime.reconciled_revision == Some(state.routing_revision);
            if same_revision && runtime.retry_at.values().all(|at| *at > Instant::now()) {
                return Ok(());
            }
            runtime.reconciled_revision = Some(state.routing_revision);
            state
                .devices
                .iter()
                .filter_map(|(id, device)| {
                    let setting = state.config.endpoints.get(id)?;
                    setting
                        .active
                        .then_some((id.clone(), (device.clone(), setting.clone())))
                })
                .collect::<HashMap<_, _>>()
        };
        runtime.retry_at.retain(|id, _| wanted.contains_key(id));
        let stopped = runtime
            .renders
            .iter()
            .filter(|(id, render)| {
                !wanted
                    .get(*id)
                    .is_some_and(|(device, _)| device.sample_rate == render.rate)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in stopped {
            if let Some(capture) = &runtime.capture {
                let _ = capture.sender.send(CaptureCommand::Remove(id.clone()));
            }
            if let Some(mut render) = runtime.renders.remove(&id) {
                render.stop()?;
            }
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .live
                .remove(&id);
            runtime.retry_at.remove(&id);
        }
        for (id, (device, setting)) in wanted {
            if runtime.renders.contains_key(&id) {
                continue;
            }
            if runtime
                .retry_at
                .get(&id)
                .is_some_and(|at| *at > Instant::now())
            {
                continue;
            }
            runtime
                .retry_at
                .insert(id.clone(), Instant::now() + Duration::from_millis(500));
            if self.stopping.load(Ordering::Acquire) {
                break;
            }
            let live = WorkerLive::new(setting.volume, setting.muted);
            let (producer, consumer, monitor) = stereo_ring(96_000);
            {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.live.insert(
                    id.clone(),
                    EndpointLive {
                        worker: live.clone(),
                        monitor,
                    },
                );
                state.recompute_latencies();
                state.queue_volume(&id, Some(setting.volume), Some(setting.muted));
            }
            pactl(&["suspend-sink", &id, "0"], Some(&self.stopping))?;
            match RenderHandle::start(id.clone(), device.sample_rate, consumer, live.clone()) {
                Ok(render) => {
                    if let Some(capture) = &runtime.capture {
                        capture
                            .sender
                            .send(CaptureCommand::Add(id.clone(), producer))
                            .map_err(|_| anyhow::anyhow!("capture thread exited"))?;
                    }
                    runtime.retry_at.remove(&id);
                    runtime.renders.insert(id, render);
                }
                Err(error) => {
                    live.set_error(Some(format!("{error:#}")));
                }
            }
        }
        Ok(())
    }

    fn apply_one_write(&self, cursor: &mut String, final_write: bool) {
        let selected = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let eligible = |id: &&String| {
                *id == &self.options.virtual_sink_name || state.devices.contains_key(*id)
            };
            state
                .writes
                .iter()
                .filter(|(id, _)| eligible(id))
                .find(|(id, _)| id.as_str() > cursor.as_str())
                .or_else(|| state.writes.iter().find(|(id, _)| eligible(id)))
                .map(|(id, write)| (id.clone(), write.clone()))
        };
        let Some((id, write)) = selected else {
            return;
        };
        *cursor = id.clone();
        let cancel = if final_write {
            None
        } else {
            Some(&self.stopping)
        };
        let volume_result = write.volume.map(|value| {
            pactl(
                &[
                    "set-sink-volume",
                    &id,
                    &format!("{}%", (value.value * 100.0).round() as u32),
                ],
                cancel,
            )
        });
        let mute_result = write.muted.map(|value| {
            pactl(
                &["set-sink-mute", &id, if value.value { "1" } else { "0" }],
                cancel,
            )
        });
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut errors = Vec::new();
        if let Some(pending) = state.writes.get_mut(&id) {
            if let (Some(value), Some(result)) = (write.volume, volume_result) {
                match result {
                    Ok(_) => {
                        if pending
                            .volume
                            .is_some_and(|pending| pending.revision == value.revision)
                        {
                            pending.volume = None;
                        }
                    }
                    Err(error) => errors.push(format!("{error:#}")),
                }
            }
            if let (Some(value), Some(result)) = (write.muted, mute_result) {
                match result {
                    Ok(_) => {
                        if pending
                            .muted
                            .is_some_and(|pending| pending.revision == value.revision)
                        {
                            pending.muted = None;
                        }
                    }
                    Err(error) => errors.push(format!("{error:#}")),
                }
            }
            let empty = pending.volume.is_none() && pending.muted.is_none();
            if empty {
                state.writes.remove(&id);
            }
        }
        let guard = state.guards.entry(id.clone()).or_default();
        if write.volume.is_some() {
            guard.volume_at = Some(Instant::now());
        }
        if write.muted.is_some() {
            guard.mute_at = Some(Instant::now());
        }
        if errors.is_empty() {
            state.write_errors.remove(&id);
        } else {
            state.write_errors.insert(id, errors.join("; "));
        }
    }

    fn move_streams(&self) -> Result<()> {
        let output = pactl(&["-f", "json", "list", "sink-inputs"], Some(&self.stopping))?;
        self.migrate_streams(&output, |index| {
            pactl(
                &[
                    "move-sink-input",
                    &index.to_string(),
                    &self.options.virtual_sink_name,
                ],
                Some(&self.stopping),
            )
            .map(|_| ())
        })
    }

    fn migrate_streams(
        &self,
        output: &[u8],
        mut move_stream: impl FnMut(u32) -> Result<()>,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct SinkInput {
            index: u32,
            #[serde(default)]
            properties: HashMap<String, String>,
        }
        let streams: Vec<SinkInput> =
            serde_json::from_slice(output).context("decode application stream list")?;
        let mut failures = Vec::new();
        for stream in streams {
            if self.stopping.load(Ordering::Acquire) {
                bail!("application stream migration cancelled");
            }
            let ours_or_router =
                ["application.name", "node.name", "media.name"]
                    .iter()
                    .any(|key| {
                        let value = stream
                            .properties
                            .get(*key)
                            .map(|value| value.to_ascii_lowercase())
                            .unwrap_or_default();
                        value.contains("pw_matrix")
                            || value.contains("pw-matrix")
                            || value.contains("pacat")
                            || value.contains("pw-play")
                    });
            if !ours_or_router {
                // Streams can disappear or reject moves independently of hub health.
                // Keep routing other applications and expose partial migration to the UI.
                if let Err(error) = move_stream(stream.index) {
                    failures.push(format!("stream {}: {error:#}", stream.index));
                }
            }
        }
        let warning = (!failures.is_empty()).then(|| {
            anyhow::anyhow!(
                "Some application streams could not be moved: {}",
                failures.join("; ")
            )
        });
        self.set_control_error("stream migration", warning.as_ref());
        Ok(())
    }

    fn cleanup(&self, runtime: &mut Runtime) -> Result<()> {
        let mut errors = Vec::new();
        self.master.stop.store(true, Ordering::Release);
        for render in runtime.renders.values() {
            render.live.stop.store(true, Ordering::Release);
        }
        if let Some(mut capture) = runtime.capture.take() {
            if let Err(error) = capture.stop() {
                errors.push(format!("{error:#}"));
            }
        }
        for (_, mut render) in runtime.renders.drain() {
            if let Err(error) = render.stop() {
                errors.push(format!("{error:#}"));
            }
        }
        if runtime.module_id.is_some() {
            // Make one final bounded attempt per coalesced endpoint, never loop forever on OS failure.
            let count = self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .writes
                .len();
            let mut cursor = String::new();
            for _ in 0..count {
                self.apply_one_write(&mut cursor, true);
            }
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state
                .writes
                .keys()
                .any(|id| id == &self.options.virtual_sink_name || state.devices.contains_key(id))
            {
                errors.push(
                    state
                        .write_errors
                        .values()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; "),
                );
            }
        }
        if runtime.default_changed && !runtime.previous_default.is_empty() {
            match pactl_text(&["get-default-sink"], None) {
                Ok(current) if current == self.options.virtual_sink_name => {
                    if let Err(error) =
                        pactl(&["set-default-sink", &runtime.previous_default], None)
                    {
                        errors.push(format!("restore previous default: {error:#}"));
                    }
                }
                Ok(_) => {}
                Err(error) => errors.push(format!(
                    "query current default before restoration: {error:#}"
                )),
            }
        }
        if let Some(module) = runtime.module_id.take() {
            match pactl_text(&["list", "short", "modules"], None) {
                Ok(modules)
                    if owned_module(
                        &modules,
                        &self.options.virtual_sink_name,
                        &runtime.owner_token,
                    )
                    .as_deref()
                        == Some(module.as_str()) =>
                {
                    if let Err(error) = pactl(&["unload-module", &module], None) {
                        errors.push(format!("release owned virtual module {module}: {error:#}"));
                    }
                }
                Ok(_) => {} // Our module was already removed; never unload a reused index.
                Err(error) => errors.push(format!(
                    "verify virtual module ownership before release: {error:#}"
                )),
            }
        }
        runtime.lock.take();
        if let Err(error) = self.flush_config() {
            errors.push(format!("{error:#}"));
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.running = false;
        state.live.clear();
        for reply in state.refresh.drain(..) {
            let _ = reply.send(Err("engine stopped".into()));
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("; "));
        }
        Ok(())
    }
}

#[derive(Default)]
struct Runtime {
    lock: Option<File>,
    module_id: Option<String>,
    owner_token: String,
    previous_default: String,
    default_changed: bool,
    capture: Option<CaptureHandle>,
    renders: HashMap<String, RenderHandle>,
    reconciled_revision: Option<u64>,
    retry_at: HashMap<String, Instant>,
}

fn pactl_text(args: &[&str], stop: Option<&AtomicBool>) -> Result<String> {
    Ok(String::from_utf8(pactl(args, stop)?)
        .context("pactl returned non-UTF8 output")?
        .trim()
        .to_owned())
}

fn owned_module(modules: &str, name: &str, token: &str) -> Option<String> {
    let sink = format!("sink_name={name}");
    let owner = format!("pw_matrix.owner={token}");
    modules
        .lines()
        .find(|line| {
            line.split_whitespace().any(|field| field == sink)
                && line
                    .split_whitespace()
                    .any(|field| field.trim_matches('"') == owner)
        })
        .and_then(|line| line.split_whitespace().next())
        .map(str::to_owned)
}

fn lock_sink(name: &str) -> Result<File> {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    let uid = unsafe { libc::getuid() };
    let path = root.join(format!("pw-matrix-{uid}-{name}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("open instance lock {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!(
            "Another PW-Matrix instance owns sink '{name}' ({})",
            std::io::Error::last_os_error()
        );
    }
    Ok(file)
}

fn owner_serial() -> usize {
    static SERIAL: AtomicUsize = AtomicUsize::new(0);
    SERIAL.fetch_add(1, Ordering::Relaxed)
}

fn feedback_allowed(last_write: Option<Instant>, pending: bool) -> bool {
    !pending && last_write.is_none_or(|instant| instant.elapsed() >= USER_GRACE)
}

fn feedback_volume(slider: f32, os_volume: f32) -> f32 {
    if slider > 1.0 && os_volume >= 0.98 {
        slider
    } else {
        clamp_volume(os_volume)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_or_recent_user_writes_cannot_be_rolled_back_by_poll() {
        assert!(!feedback_allowed(None, true));
        assert!(!feedback_allowed(Some(Instant::now()), false));
        assert!(feedback_allowed(
            Some(Instant::now() - USER_GRACE - Duration::from_millis(1)),
            false
        ));
        assert_eq!(feedback_volume(1.4, 1.0), 1.4);
        assert_eq!(feedback_volume(1.4, 0.37), 0.37);
        assert_eq!(feedback_volume(0.0, 0.0), 0.0);
    }

    #[test]
    fn coalescing_retains_final_volume_and_mute_independently() {
        let engine = AudioEngine::new();
        engine.set_master_volume(0.1);
        engine.toggle_master_mute();
        engine.set_master_volume(1.3);
        let state = engine.state.lock().unwrap();
        let write = &state.writes[DEFAULT_HUB];
        assert_eq!(write.volume.unwrap().value, 1.0);
        assert!(write.muted.unwrap().value);
        assert_eq!(state.config.master_volume, 1.3);
    }

    #[test]
    fn options_do_not_start_or_claim_audio_in_constructor() {
        let engine = AudioEngine::new();
        let snapshot = engine.snapshot();
        assert!(!snapshot.capturing);
        assert_eq!(snapshot.master_volume, 1.0);
        assert!(snapshot.auto_sync);
        assert_eq!(snapshot.target_floor_ms, 16);
        assert!(snapshot.endpoints.is_empty());
        assert!(engine.lifecycle.lock().unwrap().worker.is_none());
    }

    #[test]
    fn rejected_stream_does_not_abort_migration_and_remains_visible() {
        let engine = AudioEngine::new();
        let streams = br#"[
            {"index":74056,"sink":4294967295,"properties":{"node.name":"plasmashell","node.dont-reconnect":"true"}},
            {"index":74057,"properties":{"application.name":"Music player"}},
            {"index":74058,"properties":{"application.name":"pw-matrix-output"}}
        ]"#;
        let mut attempted = Vec::new();
        engine
            .migrate_streams(streams, |index| {
                attempted.push(index);
                if index == 74056 {
                    bail!("Failure: Invalid argument");
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(attempted, [74056, 74057]);
        let warning = engine.snapshot().error.unwrap();
        assert!(warning.contains("74056"));
        assert!(warning.contains("Invalid argument"));
        engine.migrate_streams(streams, |_| Ok(())).unwrap();
        assert!(engine.snapshot().error.is_none());
    }

    #[test]
    fn requested_six_ms_floor_keeps_eight_ms_effective_floor_during_alignment() {
        let engine = AudioEngine::new();
        let mut state = engine.state.lock().unwrap();
        state.config.target_floor_ms = 6;
        for (id, hardware_ms) in [("fast", 12.0), ("slow", 22.0)] {
            state.devices.insert(
                id.into(),
                DeviceInfo {
                    hardware_ms,
                    ..DeviceInfo::default()
                },
            );
            state.config.endpoints.insert(
                id.into(),
                EndpointSetting {
                    active: true,
                    ..EndpointSetting::default()
                },
            );
            let (_, _, monitor) = stereo_ring(2048);
            state.live.insert(
                id.into(),
                EndpointLive {
                    worker: WorkerLive::new(1.0, false),
                    monitor,
                },
            );
        }
        state.recompute_latencies();
        assert_eq!(state.config.target_floor_ms, 6);
        assert_eq!(
            state.live["fast"]
                .worker
                .latency_frames
                .load(Ordering::Relaxed),
            864
        );
        assert_eq!(
            state.live["slow"]
                .worker
                .latency_frames
                .load(Ordering::Relaxed),
            384
        );
        state.config.auto_sync_mode = false;
        state.recompute_latencies();
        assert_eq!(
            state.live["fast"]
                .worker
                .latency_frames
                .load(Ordering::Relaxed),
            384
        );
        assert_eq!(
            state.live["slow"]
                .worker
                .latency_frames
                .load(Ordering::Relaxed),
            384
        );
    }

    #[test]
    fn latency_snapshot_adds_filter_history_only_for_existing_audio_transport() {
        let engine = AudioEngine::new();
        let (mut producer, _consumer, monitor) = stereo_ring(2048);
        producer.write_stereo(&[0.25; 512]);
        {
            let mut state = engine.state.lock().unwrap();
            for id in ["live", "idle"] {
                state.devices.insert(
                    id.into(),
                    DeviceInfo {
                        hardware_ms: 12.0,
                        ..DeviceInfo::default()
                    },
                );
                state
                    .config
                    .endpoints
                    .insert(id.into(), EndpointSetting::default());
            }
            state.live.insert(
                "live".into(),
                EndpointLive {
                    worker: WorkerLive::new(1.0, false),
                    monitor,
                },
            );
        }
        let snapshot = engine.snapshot();
        let live = snapshot
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == "live")
            .unwrap();
        let idle = snapshot
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == "idle")
            .unwrap();
        assert!((live.ring_ms - 256.0 / 48.0).abs() < 1e-12);
        assert!((live.latency_ms - live.ring_ms - 12.0 - FILTER_DELAY_MS).abs() < 1e-12);
        assert_eq!(idle.ring_ms, 0.0);
        assert_eq!(idle.latency_ms, 12.0);
    }
}
