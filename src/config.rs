//! Reading and writing `cosmic-config` entries.
//!
//! COSMIC keeps each setting as its own file of RON text:
//!
//! ```text
//! ~/.config/cosmic/com.system76.CosmicAppletTime/v1/military_time   ->  true
//! ~/.config/cosmic/com.system76.CosmicAppletTime/v1/first_day_of_week ->  0
//! ```
//!
//! Values are written verbatim: the caller supplies RON, so strings keep their
//! quotes and enums keep their variant names. Nothing here parses RON, which
//! keeps the tool honest about what it can and cannot validate.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Directories are named after the app id, keys are file names: anything with a
/// path separator could escape the config tree, so refuse it.
fn check_component(kind: &str, value: &str) -> Result<()> {
    if value.is_empty() || value == "." || value == ".." {
        bail!("{kind} must be a non-empty name, got {value:?}");
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("{kind} {value:?} may only contain letters, digits, '.', '-', '_'");
    }
    Ok(())
}

fn config_root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME").context("neither XDG_CONFIG_HOME nor HOME is set")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("cosmic"))
}

fn app_dir(app: &str) -> Result<PathBuf> {
    check_component("app id", app)?;
    Ok(config_root()?.join(app).join("v1"))
}

pub fn read(app: Option<&str>, key: Option<&str>) -> Result<Value> {
    let Some(app) = app else {
        return Ok(json!({"apps": list_apps()?}));
    };
    let dir = app_dir(app)?;

    match key {
        Some(key) => {
            check_component("key", key)?;
            let path = dir.join(key);
            match std::fs::read_to_string(&path) {
                Ok(raw) => Ok(json!({
                    "app": app,
                    "key": key,
                    "value": raw.trim(),
                    "path": path.display().to_string(),
                })),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    bail!("{} does not exist", path.display())
                }
                Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
            }
        }
        None => {
            let entries = std::fs::read_dir(&dir).with_context(|| {
                format!(
                    "{} does not exist — is {app:?} the right app id?",
                    dir.display()
                )
            })?;
            let mut keys = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let key = entry.file_name().to_string_lossy().into_owned();
                let raw = std::fs::read_to_string(&path).unwrap_or_default();
                keys.push(json!({"key": key, "value": raw.trim()}));
            }
            keys.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
            Ok(json!({"app": app, "dir": dir.display().to_string(), "entries": keys}))
        }
    }
}

fn list_apps() -> Result<Vec<String>> {
    let root = config_root()?;
    let mut apps = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                apps.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    apps.sort();
    Ok(apps)
}

/// Writes `value` verbatim, or deletes the key when `value` is `None`.
///
/// Returns the previous content so a caller can put it back. COSMIC watches its
/// config tree, so most consumers pick the change up immediately; a few applets
/// only re-read at startup, which is what `restart_applet` is for.
pub fn write(app: &str, key: &str, value: Option<&str>) -> Result<Value> {
    let dir = app_dir(app)?;
    check_component("key", key)?;
    let path = dir.join(key);
    let before = std::fs::read_to_string(&path).ok();

    match value {
        Some(raw) => {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("create {}", dir.display()))?;
            write_atomic(&path, raw)?;
            Ok(json!({
                "app": app,
                "key": key,
                "written": raw,
                "previous": before.map(|text| text.trim().to_string()),
                "path": path.display().to_string(),
            }))
        }
        None => {
            if before.is_none() {
                bail!("{} does not exist, nothing to delete", path.display());
            }
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            Ok(json!({
                "app": app,
                "key": key,
                "deleted": true,
                "previous": before.map(|text| text.trim().to_string()),
                "path": path.display().to_string(),
            }))
        }
    }
}

/// Writes through a temporary file in the same directory, so a watcher never
/// observes a half-written value.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().context("config path has no parent")?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, contents).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}
