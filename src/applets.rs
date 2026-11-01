//! COSMIC panel applets, as seen from outside.
//!
//! Every applet is an ordinary process started by `cosmic-panel`. The panel
//! does **not** respawn one that exits — despite launch-pad's
//! `RestartMode::ExponentialBackoff` in `cosmic-panel-bin/src/main.rs`, sending
//! SIGTERM leaves the slot empty. What does bring an applet back is the panel's
//! config watcher: rewriting the `plugins_*` key that lists it makes the panel
//! tear the section down and spawn it again. That is the mechanism `restart`
//! uses, and it is why this module writes config files instead of sending
//! signals.
//!
//! The applets ship as one multi-call binary (`bin/.cosmic-applets`) with a
//! symlink per applet, so the process name has to come from `argv[0]`, not from
//! `/proc/<pid>/comm` (which reads `.cosmic-applets` for all of them).

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

const APPLET_PREFIXES: [&str; 3] = ["cosmic-applet-", "cosmic-app-list", "cosmic-panel-button"];

/// Panel apps whose `plugins_*` keys hold the applet lists.
const PANEL_APPS: [&str; 2] = [
    "com.system76.CosmicPanel.Panel",
    "com.system76.CosmicPanel.Dock",
];

#[derive(Clone, Debug, Serialize)]
pub struct Applet {
    pub pid: u32,
    /// `argv[0]` basename, e.g. `cosmic-applet-time`.
    pub name: String,
    /// Remaining arguments; panel buttons carry their app id here.
    pub args: Vec<String>,
    pub ppid: Option<u32>,
    pub parent: Option<String>,
    pub rss_kb: Option<u64>,
    /// Seconds since the process started.
    pub age_s: Option<u64>,
}

pub fn list() -> Result<Vec<Applet>> {
    let mut applets = Vec::new();
    let entries = std::fs::read_dir("/proc").context("read /proc")?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(applet) = read_applet(pid) {
            applets.push(applet);
        }
    }
    applets.sort_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    Ok(applets)
}

fn read_applet(pid: u32) -> Option<Applet> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut argv: Vec<String> = cmdline
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    if argv.is_empty() {
        return None;
    }
    let name = basename(&argv.remove(0));
    if !APPLET_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return None;
    }

    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok();
    let ppid = status
        .as_deref()
        .and_then(|status| field(status, "PPid:"))
        .and_then(|value| value.parse().ok());
    let rss_kb = status
        .as_deref()
        .and_then(|status| field(status, "VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok());

    Some(Applet {
        pid,
        name,
        args: argv,
        ppid,
        parent: ppid.and_then(process_name),
        rss_kb,
        age_s: age_seconds(pid),
    })
}

fn basename(path: &str) -> String {
    PathBuf::from(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn field<'a>(status: &'a str, key: &str) -> Option<&'a str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(key).map(str::trim))
}

fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let first = cmdline
        .split(|byte| *byte == 0)
        .next()
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .unwrap_or_default();
    if first.is_empty() {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|comm| comm.trim().to_string())
            .ok()
            .filter(|comm| !comm.is_empty())
    } else {
        Some(basename(&first))
    }
}

/// Seconds since the process started, from the starttime field of
/// `/proc/<pid>/stat` (in clock ticks since boot) against `/proc/uptime`.
fn age_seconds(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can contain spaces and parentheses, so split after the last ')'.
    let rest = &stat[stat.rfind(')')? + 2..];
    let starttime: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = if ticks > 0 { ticks as u64 } else { 100 };
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let started = starttime / hz;
    Some((uptime as u64).saturating_sub(started))
}

/// Restarts every applet whose name matches `selector`.
///
/// `selector` is a full name (`cosmic-applet-time`) or the suffix after
/// `cosmic-applet-` (`time`). All matching instances restart — there is one per
/// panel, so two monitors mean two copies.
///
/// The restart goes through the panel's `plugins_*` config: the key is blanked
/// and then written back, which is the only thing observed to make the panel
/// respawn an applet. In practice the panel then rebuilds **every** applet on
/// that section — time, workspaces and audio all come back with new pids — so
/// this is a panel refresh, not a single-applet restart. Backups are written
/// before any config is touched, and a drop guard restores the original text
/// if blanking or the respawn path returns early.
pub fn restart(selector: &str) -> Result<(Vec<Applet>, Vec<PathBuf>)> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("selector must not be empty");
    }
    let matches = |applet: &Applet| -> bool {
        applet.name == selector
            || applet.name == format!("cosmic-applet-{selector}")
            || applet.name.ends_with(&format!("-{selector}"))
    };

    let before: Vec<Applet> = list()?.into_iter().filter(matches).collect();
    if before.is_empty() {
        bail!("no running applet matches {selector:?}");
    }
    let wanted = plugin_candidates(&before);

    let mut targets: Vec<(PathBuf, String)> = Vec::new();
    for file in panel_config_files()? {
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("read panel config {}", file.display()))?;
        let listed: BTreeSet<String> = quoted_strings(&text).into_iter().collect();
        if listed.iter().any(|entry| wanted.contains(entry)) {
            targets.push((file, text));
        }
    }
    if targets.is_empty() {
        bail!(
            "found {} applet(s) named {selector:?} but no panel config lists them; refusing to \
             touch anything. Looked for {:?} under {}",
            before.len(),
            wanted,
            crate::config::config_root()
                .map(|root| root.display().to_string())
                .unwrap_or_default()
        );
    }

    // Keep a copy outside the config tree before touching anything.
    let mut backups = Vec::new();
    for (file, text) in &targets {
        let path = backup(file, text)
            .with_context(|| format!("back up {} before blanking it", file.display()))?;
        backups.push(path);
    }

    let mut restore = PanelRestore::new(targets);
    let files: Vec<PathBuf> = restore
        .targets
        .iter()
        .map(|(file, _)| file.clone())
        .collect();
    for file in &files {
        if let Err(err) = crate::config::write_atomic(file, "None") {
            let restore_failures = restore.restore();
            if restore_failures.is_empty() {
                bail!(
                    "failed to blank {}: {err:#}; the panel config was restored from backups in {}",
                    file.display(),
                    backup_dir()
                        .map(|dir| dir.display().to_string())
                        .unwrap_or_default()
                );
            }
            bail!(
                "failed to blank {}: {err:#}; FAILED to restore the panel config: {}. Backups are \
                 in {}",
                file.display(),
                restore_failures.join("; "),
                backup_dir()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default()
            );
        }
    }
    // The config watcher debounces, so give it time to see the blank value.
    std::thread::sleep(Duration::from_millis(1200));

    let restore_failures = restore.restore();
    if !restore_failures.is_empty() {
        bail!(
            "FAILED to restore the panel config after blanking it — the panel is now missing the \
             applets from: {}. Backups are in {}",
            restore_failures.join("; "),
            backup_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default()
        );
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let now: Vec<Applet> = list()?.into_iter().filter(matches).collect();
        // The panel respawns one applet per screen, and they do not come up at
        // the same instant: wait until every old pid is gone and at least one
        // replacement exists, then report the freshest listing.
        let old_gone = before
            .iter()
            .all(|old| !now.iter().any(|new| new.pid == old.pid));
        if old_gone && !now.is_empty() {
            std::thread::sleep(Duration::from_millis(400));
            return Ok((list()?.into_iter().filter(matches).collect(), backups));
        }
        if Instant::now() >= deadline {
            bail!(
                "rewrote {} but the {selector:?} processes were not replaced within 10s; the \
                 config has been restored, so they should come back on the next panel reload",
                files
                    .iter()
                    .map(|file| file.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}

/// Restores the original panel config on every early return, including a panic
/// during the debounce wait. Explicit restoration still reports failures; if it
/// fails, `Drop` makes one more best-effort attempt.
struct PanelRestore {
    targets: Vec<(PathBuf, String)>,
    armed: bool,
}

impl PanelRestore {
    fn new(targets: Vec<(PathBuf, String)>) -> Self {
        Self {
            targets,
            armed: true,
        }
    }

    fn restore(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        for (file, text) in &self.targets {
            if let Err(err) = crate::config::write_atomic(file, text) {
                failures.push(format!("{}: {err:#}", file.display()));
            }
        }
        if failures.is_empty() {
            self.armed = false;
        }
        failures
    }
}

impl Drop for PanelRestore {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.restore();
        }
    }
}

/// Panel entries that could name these applets: `cosmic-applet-time` is listed
/// as `com.system76.CosmicAppletTime`, panel buttons carry their app id in
/// `args`, and app-list is `com.system76.CosmicAppList`.
fn plugin_candidates(applets: &[Applet]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for applet in applets {
        for arg in &applet.args {
            out.insert(arg.clone());
        }
        if let Some(short) = applet.name.strip_prefix("cosmic-applet-") {
            out.insert(format!("com.system76.CosmicApplet{}", camel_case(short)));
        }
        if applet.name == "cosmic-app-list" {
            out.insert("com.system76.CosmicAppList".to_string());
        }
    }
    out
}

fn camel_case(name: &str) -> String {
    name.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Every double-quoted string in a RON document, unescaped enough for plugin
/// names (they are plain ASCII identifiers).
fn quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else {
            break;
        };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    out
}

fn panel_config_files() -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let root = crate::config::config_root()?;
    for app in PANEL_APPS {
        let dir = root.join(app).join("v1");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("read panel config directory {}", dir.display()));
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("read an entry in {}", dir.display()))?;
            if !entry
                .file_type()
                .with_context(|| format!("inspect {}", entry.path().display()))?
                .is_file()
            {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("plugins_") {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn state_base_from(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
    if let Some(dir) = xdg_state_home.filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            bail!("XDG_STATE_HOME is not absolute: {}", dir.display());
        }
        return Ok(dir);
    }

    let home = home
        .filter(|value| !value.is_empty())
        .context("neither XDG_STATE_HOME nor HOME is set")?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        bail!("HOME is not absolute: {}", home.display());
    }
    Ok(home.join(".local/state"))
}

fn backup_dir() -> Result<PathBuf> {
    let xdg_state_home = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    Ok(state_base_from(xdg_state_home.as_deref(), home.as_deref())?
        .join("cosmic-desktop-mcp/panel-backups"))
}

fn backup(file: &Path, text: &str) -> Result<PathBuf> {
    backup_at(&backup_dir()?, file, text)
}

fn backup_at(dir: &Path, file: &Path, text: &str) -> Result<PathBuf> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, Permissions::from_mode(0o700))
        .with_context(|| format!("make {} private", dir.display()))?;
    let name = file
        .parent()
        .and_then(Path::file_name)
        .map(|app| app.to_string_lossy().into_owned())
        .unwrap_or_default();
    let key = file
        .file_name()
        .map(|key| key.to_string_lossy().into_owned())
        .unwrap_or_default();
    let path = dir.join(format!("{name}__{key}"));
    write_private_atomic(&path, text)?;
    Ok(path)
}

fn write_private_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().context("backup path has no parent")?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{}.{}-{stamp}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
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
        std::fs::set_permissions(path, Permissions::from_mode(0o600))
            .with_context(|| format!("make {} private", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cosmic-desktop-mcp-applets-{}-{}",
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
    fn panel_restore_puts_the_original_config_back() {
        let temp = TempDir::new();
        let file = temp.0.join("plugins_center");
        std::fs::write(&file, "[\"com.system76.CosmicAppletTime\"]").unwrap();
        let original = std::fs::read_to_string(&file).unwrap();

        let mut restore = PanelRestore::new(vec![(file.clone(), original.clone())]);
        crate::config::write_atomic(&file, "None").unwrap();

        assert!(restore.restore().is_empty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        assert!(!restore.armed);
    }

    #[test]
    fn panel_restore_drop_recovers_an_armed_transaction() {
        let temp = TempDir::new();
        let file = temp.0.join("plugins_wings");
        let original = "[\"com.system76.CosmicAppletAudio\"]".to_string();
        std::fs::write(&file, &original).unwrap();

        {
            let _restore = PanelRestore::new(vec![(file.clone(), original.clone())]);
            crate::config::write_atomic(&file, "None").unwrap();
        }

        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }

    #[test]
    fn panel_backups_are_private_and_complete() {
        let temp = TempDir::new();
        let source = temp.0.join("plugins_center");
        let backup_dir = temp.0.join("backups");
        let original = "[\"com.system76.CosmicAppletTime\"]";

        let path = backup_at(&backup_dir, &source, original).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let dir_mode = std::fs::metadata(&backup_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
