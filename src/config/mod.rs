//! XDG configuration storage. The engine's single control thread serializes saves.

pub mod schema;
pub(crate) use schema::clamp_volume;
pub use schema::{EndpointSetting, PersistentConfig};

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const LEGACY_NAME: &str = "pipewire_matrix_config.json";
static SAVE_SERIAL: AtomicU64 = AtomicU64::new(0);

pub(crate) fn default_path() -> PathBuf {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    root.join("pw-matrix").join("config.json")
}

pub(crate) fn load(path: &Path, import_legacy: bool) -> Result<(PersistentConfig, bool)> {
    match fs::read(path) {
        Ok(bytes) => return decode(&bytes, path).map(|cfg| (cfg, false)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("read configuration {}", path.display()));
        }
    }
    if import_legacy {
        let mut candidates = Vec::new();
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            candidates.push(dir.join(LEGACY_NAME));
        }
        if let Ok(cwd) = std::env::current_dir() {
            let candidate = cwd.join(LEGACY_NAME);
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
        for candidate in candidates {
            match fs::read(&candidate) {
                Ok(bytes) => return decode(&bytes, &candidate).map(|cfg| (cfg, true)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("import legacy configuration {}", candidate.display())
                    });
                }
            }
        }
    }
    Ok((PersistentConfig::default(), false))
}

fn decode(bytes: &[u8], path: &Path) -> Result<PersistentConfig> {
    let mut config: PersistentConfig = serde_json::from_slice(bytes).with_context(|| {
        format!(
            "parse configuration {} (file left unchanged)",
            path.display()
        )
    })?;
    config.validate();
    Ok(config)
}

pub(crate) fn save(path: &Path, config: &PersistentConfig) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create configuration directory {}", parent.display()))?;
    let serial = SAVE_SERIAL.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .context("configuration path has no filename")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), serial));
    let result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        // The serializer emits small writes; buffer them off the audio path.
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, config)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("save configuration {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_save_preserves_disconnected_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/config.json");
        let mut config = PersistentConfig::default();
        config.endpoints.insert(
            "disconnected".into(),
            EndpointSetting {
                active: true,
                volume: 0.0,
                ..Default::default()
            },
        );
        save(&path, &config).unwrap();
        config.master_volume = 1.4;
        save(&path, &config).unwrap();
        let (loaded, imported) = load(&path, false).unwrap();
        assert!(!imported);
        assert_eq!(loaded.master_volume, 1.4);
        assert!(loaded.endpoints["disconnected"].active);
        assert_eq!(loaded.endpoints["disconnected"].volume, 0.0);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn malformed_existing_file_is_not_replaced_with_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        fs::write(&path, "broken").unwrap();
        assert!(load(&path, false).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "broken");
    }
}
