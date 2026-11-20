//! Persistent preferences, including endpoints which are currently disconnected.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointSetting {
    pub name: String,
    pub active: bool,
    pub volume: f32,
    pub muted: bool,
    pub extra_offset_ms: i32,
}

impl Default for EndpointSetting {
    fn default() -> Self {
        Self {
            name: String::new(),
            active: false,
            volume: 1.0,
            muted: false,
            extra_offset_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistentConfig {
    pub target_floor_ms: u32,
    pub auto_sync_mode: bool,
    pub master_volume: f32,
    pub master_muted: bool,
    pub endpoints: HashMap<String, EndpointSetting>,
}

impl Default for PersistentConfig {
    fn default() -> Self {
        Self {
            target_floor_ms: 16,
            auto_sync_mode: true,
            master_volume: 1.0,
            master_muted: false,
            endpoints: HashMap::new(),
        }
    }
}

pub(crate) fn clamp_volume(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.5)
    } else {
        1.0
    }
}

impl PersistentConfig {
    pub(crate) fn validate(&mut self) {
        self.target_floor_ms = self.target_floor_ms.clamp(6, 100);
        self.master_volume = clamp_volume(self.master_volume);
        for setting in self.endpoints.values_mut() {
            setting.volume = clamp_volume(setting.volume);
            setting.extra_offset_ms = setting.extra_offset_ms.clamp(0, 1000);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_use_real_defaults_and_zero_volume_survives() {
        let mut cfg: PersistentConfig =
            serde_json::from_str(r#"{"master_volume":0,"endpoints":{"gone":{"active":true}}}"#)
                .unwrap();
        cfg.validate();
        assert_eq!(cfg.master_volume, 0.0);
        assert_eq!(cfg.target_floor_ms, 16);
        assert!(cfg.auto_sync_mode);
        assert_eq!(cfg.endpoints["gone"].volume, 1.0);
        assert!(cfg.endpoints["gone"].active);
    }

    #[test]
    fn invalid_values_cannot_reach_audio_atoms() {
        let mut cfg = PersistentConfig::default();
        cfg.master_volume = f32::NAN;
        cfg.target_floor_ms = 0;
        cfg.endpoints.insert(
            "device".into(),
            EndpointSetting {
                volume: f32::INFINITY,
                extra_offset_ms: -5,
                ..Default::default()
            },
        );
        cfg.validate();
        assert_eq!(cfg.master_volume, 1.0);
        assert_eq!(cfg.target_floor_ms, 6);
        assert_eq!(cfg.endpoints["device"].volume, 1.0);
        assert_eq!(cfg.endpoints["device"].extra_offset_ms, 0);
    }
}
