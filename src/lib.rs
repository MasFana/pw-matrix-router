//! Audio Router - PipeWire Matrix Hi-Res Multi-DAC Audio Router
//!
//! GPUI Kit Rust desktop application for real-time audio routing.
//! Core DSP module ported from Go main_linux.go

// `as_chunks` is still unstable in std, so stable `chunks_exact` stays in the hot loops.
#![allow(clippy::chunks_exact_to_as_chunks)]

pub mod audio;
pub mod components;
pub mod config;
pub mod dsp;
pub mod platform;
pub mod ui;
