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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    pub ppid: u32,
    pub parent: String,
    pub rss_kb: u64,
    /// Seconds since the process started.
    pub age_s: u64,
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

    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let ppid = field(&status, "PPid:")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let rss_kb = field(&status, "VmRSS:")
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    Some(Applet {
        pid,
        name,
        args: argv,
        ppid,
        parent: process_name(ppid),
        rss_kb,
        age_s: age_seconds(pid).unwrap_or(0),
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

fn process_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let first = cmdline
        .split(|byte| *byte == 0)
        .next()
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .unwrap_or_default();
    if first.is_empty() {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|comm| comm.trim().to_string())
            .unwrap_or_default()
    } else {
        basename(&first)
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
/// this is a panel refresh, not a single-applet restart. The original text is
/// written back unconditionally, even when the respawn does not show up in
/// time.
pub fn restart(selector: &str) -> Result<Vec<Applet>> {
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
    for file in panel_config_files() {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
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
            config_root()
                .map(|root| root.display().to_string())
                .unwrap_or_default()
        );
    }

    // Keep a copy outside the config tree: if writing back fails halfway, the
    // user can still recover a whole panel section by hand.
    for (file, text) in &targets {
        let _ = backup(file, text);
        std::fs::write(file, "None").with_context(|| format!("blank {}", file.display()))?;
    }
    // The config watcher debounces, so give it time to see the blank value.
    std::thread::sleep(Duration::from_millis(1200));

    let mut restore_failures = Vec::new();
    for (file, text) in &targets {
        if let Err(err) = std::fs::write(file, text) {
            restore_failures.push(format!("{}: {err}", file.display()));
        }
    }
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
            return Ok(list()?.into_iter().filter(matches).collect());
        }
        if Instant::now() >= deadline {
            bail!(
                "rewrote {} but the {selector:?} processes were not replaced within 10s; the \
                 config has been restored, so they should come back on the next panel reload",
                targets
                    .iter()
                    .map(|(file, _)| file.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
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

fn config_root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home =
                std::env::var_os("HOME").context("neither XDG_CONFIG_HOME nor HOME is set")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("cosmic"))
}

fn panel_config_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(root) = config_root() else {
        return files;
    };
    for app in PANEL_APPS {
        let dir = root.join(app).join("v1");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("plugins_") {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files
}

fn backup_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/cosmic-desktop-mcp/panel-backups"))
}

fn backup(file: &Path, text: &str) -> Result<PathBuf> {
    let dir = backup_dir().context("HOME is not set")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
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
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}
