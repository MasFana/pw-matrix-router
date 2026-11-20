//! Native Linux multi-output audio service, independent of the UI toolkit.

mod capture;
mod device;
mod engine;
mod render;

pub use engine::{AudioEngine, EndpointSnapshot, EngineOptions, EngineSnapshot};
