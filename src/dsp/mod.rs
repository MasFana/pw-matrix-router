//! Native audio DSP: ownership-safe stereo transport, adaptive resampling,
//! hybrid volume, and Go-compatible stateful stereo telemetry.

mod resampling;
pub mod ring_buffer;
pub mod telem;
pub mod volume;

pub use ring_buffer::{RingConsumer, RingMonitor, RingProducer, stereo_ring};
pub use telem::{TelemetryAnalyzer, TelemetrySnapshot};
pub use volume::{effective_total_gain, soft_limit, split_hybrid_volume};
