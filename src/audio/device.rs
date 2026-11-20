//! Pulse-compatible sink discovery and sample-format parsing.

use crate::platform::process::pactl;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

/// `pacat`/`pw-play` playback buffer requested by the renderer, in milliseconds.
const REQUESTED_RENDER_MS: f64 = 12.0;

#[derive(Debug, Clone, Default)]
pub(crate) struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits: u32,
    pub hardware_ms: f64,
}

#[derive(Deserialize)]
struct SinkJson {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    sample_specification: String,
    #[serde(default)]
    mute: bool,
    #[serde(default)]
    volume: HashMap<String, ChannelVolume>,
    #[serde(default)]
    properties: HashMap<String, String>,
}

#[derive(Deserialize)]
struct ChannelVolume {
    #[serde(default)]
    value_percent: String,
}

pub(crate) struct PolledEndpoint {
    pub info: DeviceInfo,
    pub volume: f32,
    pub muted: bool,
    pub virtual_sink: bool,
    pub router_sink: bool,
}

pub(crate) fn query_sinks(default: &str, stop: &AtomicBool) -> Result<Vec<PolledEndpoint>> {
    let output = pactl(&["-f", "json", "list", "sinks"], Some(stop))?;
    let raw: Vec<SinkJson> = serde_json::from_slice(&output).context("decode pactl sink list")?;
    Ok(raw
        .into_iter()
        .filter(|sink| !sink.name.is_empty())
        .map(|sink| {
            let (sample_rate, channels, bits) = parse_sample_spec(&sink.sample_specification);
            let name = if !sink.description.is_empty() {
                sink.description
            } else {
                sink.properties
                    .get("node.nick")
                    .or_else(|| sink.properties.get("device.description"))
                    .cloned()
                    .unwrap_or_else(|| sink.name.clone())
            };
            let mut sum = 0.0;
            let mut count = 0;
            for value in sink.volume.values() {
                if let Ok(percent) = value
                    .value_percent
                    .trim()
                    .trim_end_matches('%')
                    .parse::<f32>()
                    && percent.is_finite()
                {
                    sum += percent;
                    count += 1;
                }
            }
            let volume = if count == 0 {
                1.0
            } else {
                (sum / count as f32 / 100.0).clamp(0.0, 1.5)
            };
            // The renderer requests this playback buffer; it is an estimate, not a
            // hardware measurement. pactl's live latency changes with stream state
            // and must not drive the drift servo.
            let hardware_ms = REQUESTED_RENDER_MS;
            let virtual_sink = sink
                .properties
                .get("device.class")
                .is_some_and(|class| class == "abstract")
                || sink
                    .properties
                    .get("node.virtual")
                    .is_some_and(|value| value == "true")
                || sink.name.starts_with("pw_matrix_");
            let router_sink = sink.name == "pw_matrix_virtual_sink"
                || sink.properties.contains_key("pw_matrix.owner");
            PolledEndpoint {
                info: DeviceInfo {
                    is_default: sink.name == default,
                    id: sink.name,
                    name,
                    sample_rate,
                    channels,
                    bits,
                    hardware_ms,
                },
                volume,
                muted: sink.mute,
                virtual_sink,
                router_sink,
            }
        })
        .collect())
}

fn parse_sample_spec(spec: &str) -> (u32, u32, u32) {
    let (mut rate, mut channels, mut bits) = (48_000, 2, 32);
    for token in spec.split_whitespace() {
        if let Some(value) = token
            .strip_suffix("Hz")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
        {
            rate = value;
        } else if let Some(value) = token
            .strip_suffix("ch")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
        {
            channels = value;
        } else if token.starts_with("s16") {
            bits = 16;
        } else if token.starts_with("s24") {
            bits = 24;
        } else if token.starts_with("s32") || token.starts_with("float32") {
            bits = 32;
        } else if token.starts_with("u8") {
            bits = 8;
        }
    }
    (rate, channels, bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pulse_sample_specs_use_actual_rate_and_format() {
        assert_eq!(parse_sample_spec("s16le 2ch 44100Hz"), (44_100, 2, 16));
        assert_eq!(parse_sample_spec("float32le 2ch 48000Hz"), (48_000, 2, 32));
        assert_eq!(parse_sample_spec("s24-32le 6ch 96000Hz"), (96_000, 6, 24));
        assert_eq!(parse_sample_spec("invalid"), (48_000, 2, 32));
    }
}
