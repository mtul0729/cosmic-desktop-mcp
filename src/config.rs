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

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

fn config_root_from(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
    let base = if let Some(dir) = xdg_config_home.filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            bail!("XDG_CONFIG_HOME is not absolute: {}", dir.display());
        }
        dir
    } else {
        let home = home
            .filter(|value| !value.is_empty())
            .context("neither XDG_CONFIG_HOME nor HOME is set")?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            bail!("HOME is not absolute: {}", home.display());
        }
        home.join(".config")
    };
    Ok(base.join("cosmic"))
}

pub(crate) fn config_root() -> Result<PathBuf> {
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    config_root_from(xdg_config_home.as_deref(), home.as_deref())
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
            let keys = read_entries(&dir).with_context(|| {
                format!(
                    "read config for {app:?} from {} — is that the right app id?",
                    dir.display()
                )
            })?;
            Ok(json!({"app": app, "dir": dir.display().to_string(), "entries": keys}))
        }
    }
}

fn read_entries(dir: &Path) -> Result<Vec<Value>> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?;
    let mut keys = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read an entry in {}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let key = entry.file_name().to_string_lossy().into_owned();
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read config value {}", path.display()))?;
        keys.push(json!({"key": key, "value": raw.trim()}));
    }
    keys.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    Ok(keys)
}

fn list_apps() -> Result<Vec<String>> {
    let root = config_root()?;
    let mut apps = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(apps),
        Err(err) => {
            return Err(err).with_context(|| format!("read config root {}", root.display()));
        }
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read an entry in {}", root.display()))?;
        if entry.path().is_dir() {
            apps.push(entry.file_name().to_string_lossy().into_owned());
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
    let before = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| {
                format!("read previous value from {} before writing", path.display())
            });
        }
    };

    match value {
        Some(raw) => {
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
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
pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().context("config path has no parent")?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{}.{}-{stamp}-{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default(),
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cosmic-desktop-mcp-config-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn unreadable_values_are_errors_not_empty_strings() {
        let temp = TempDir::new();
        std::fs::write(temp.0.join("broken"), [0xff, 0xfe]).unwrap();

        let err = read_entries(&temp.0).unwrap_err().to_string();

        assert!(err.contains("broken"), "{err}");
    }

    #[test]
    fn relative_config_roots_are_rejected() {
        assert!(config_root_from(Some(OsStr::new("config")), None).is_err());
        assert!(config_root_from(None, Some(OsStr::new("home"))).is_err());
        assert_eq!(
            config_root_from(Some(OsStr::new("")), Some(OsStr::new("/home/tester"))).unwrap(),
            PathBuf::from("/home/tester/.config/cosmic")
        );
    }

    #[test]
    fn atomic_writes_are_private_and_leave_no_temporary_file() {
        let temp = TempDir::new();
        let path = temp.0.join("setting");

        write_atomic(&path, "true").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "true");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let entries: Vec<_> = std::fs::read_dir(&temp.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![OsStr::new("setting")]);
    }
}
