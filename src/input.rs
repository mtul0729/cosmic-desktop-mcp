//! Input dispatch.
//!
//! Two backends exist because they trade differently:
//!
//! - **eis** (default) — libei through the RemoteDesktop portal. No device
//!   permission needed, logical coordinates from the compositor, works on all
//!   outputs including mixed scale factors. Costs one authorization dialog per
//!   machine (remembered), and **a click does not move keyboard focus** on
//!   COSMIC, so `click` followed by `type_text` only lands where focus already is.
//! - **uinput** — a synthetic kernel device. Needs write access to
//!   `/dev/uinput`, transfers focus like a real mouse, but its absolute range is
//!   built from one output's physical pixels and it cannot express a second
//!   screen correctly.
//!
//! `COSMIC_DESKTOP_INPUT=uinput` switches; anything else defaults to eis.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::eis;
use crate::uinput::{self, Button, Device};
use crate::wl::capture;

static UINPUT: OnceLock<Mutex<Device>> = OnceLock::new();
static OUTPUTS: OnceLock<Vec<capture::OutputInfo>> = OnceLock::new();

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Eis,
    Uinput,
}

fn mode() -> Result<Mode> {
    match std::env::var("COSMIC_DESKTOP_INPUT").ok().as_deref() {
        None | Some("") | Some("eis") => Ok(Mode::Eis),
        Some("uinput") => Ok(Mode::Uinput),
        Some(other) => bail!(
            "unknown COSMIC_DESKTOP_INPUT={other:?}; expected \"eis\" or \"uinput\""
        ),
    }
}

pub fn describe() -> Result<String> {
    let mode = mode()?;
    let mut out = match mode {
        Mode::Eis => "backend: eis (libei through the RemoteDesktop portal)\n".to_string(),
        Mode::Uinput => "backend: uinput (/dev/uinput synthetic device)\n".to_string(),
    };
    match mode {
        Mode::Eis => out.push_str(&eis::session()?.describe()),
        Mode::Uinput => {
            let device = uinput_device()?;
            out.push_str(&format!("uinput device: {}x{}\n", device.size().0, device.size().1));
        }
    }
    Ok(out)
}

/// Moves the pointer. `x`/`y` are pixels in the screenshot of `output`.
pub fn move_to(x: i32, y: i32, output: usize) -> Result<()> {
    match mode()? {
        Mode::Eis => {
            let (lx, ly) = logical(x, y, output)?;
            eis::session()?.move_to(lx, ly)
        }
        Mode::Uinput => {
            single_output(output)?;
            uinput_device()?.move_to(x, y)
        }
    }
}

pub fn click(button: Button, x: i32, y: i32, output: usize, count: u32) -> Result<()> {
    move_to(x, y, output)?;
    // A press that lands during the move is read as the end of a drag; the
    // compositor needs to see a settled hover first.
    std::thread::sleep(Duration::from_millis(250));

    for round in 0..count.max(1) {
        if round > 0 {
            std::thread::sleep(Duration::from_millis(60));
        }
        set_button(button, true)?;
        std::thread::sleep(Duration::from_millis(90));
        set_button(button, false)?;
    }
    Ok(())
}

pub fn scroll(x: i32, y: i32, output: usize, dx: i32, dy: i32) -> Result<()> {
    move_to(x, y, output)?;
    std::thread::sleep(Duration::from_millis(150));

    match mode()? {
        Mode::Eis => {
            // EIS scroll axes follow the touchpad convention: positive y is
            // down, while the tool's dy is in wheel clicks with positive = up
            // (the uinput REL_WHEEL convention).
            eis::session()?.scroll_clicks(dx, -dy)
        }
        Mode::Uinput => uinput_device()?.scroll(dy, dx),
    }
}

pub fn key(code: u16, pressed: bool) -> Result<()> {
    match mode()? {
        Mode::Eis => eis::session()?.key(u32::from(code), pressed),
        Mode::Uinput => uinput_device()?.key(code, pressed),
    }
}

/// Presses a `ctrl+s` style chord: modifiers in the given order, released in
/// reverse.
pub fn chord(combo: &str) -> Result<()> {
    let codes = uinput::parse_chord(combo)?;
    for code in &codes {
        key(*code, true)?;
    }
    for code in codes.iter().rev() {
        key(*code, false)?;
    }
    Ok(())
}

/// Types printable ASCII as keycodes. Subject to the active input method —
/// see the note in `tool_type_text`.
pub fn type_keys(text: &str) -> Result<()> {
    for ch in text.chars() {
        let (code, shift) =
            uinput::keycode_for(ch).with_context(|| format!("character {ch:?} needs an input method"))?;
        if shift {
            key(uinput::KEY_LEFTSHIFT, true)?;
        }
        key(code, true)?;
        key(code, false)?;
        if shift {
            key(uinput::KEY_LEFTSHIFT, false)?;
        }
    }
    Ok(())
}

fn set_button(button: Button, pressed: bool) -> Result<()> {
    match mode()? {
        Mode::Eis => eis::session()?.button(u32::from(button.code()), pressed),
        Mode::Uinput => uinput_device()?.button(button, pressed),
    }
}

fn single_output(output: usize) -> Result<()> {
    if output != 0 {
        bail!(
            "the uinput backend only addresses output 0; set COSMIC_DESKTOP_INPUT=eis for \
             multi-monitor coordinates"
        );
    }
    Ok(())
}

/// Maps a screenshot pixel of `output` to a logical desktop coordinate.
///
/// The two spaces are not interchangeable: a screenshot measures physical
/// pixels, EIS speaks logical ones. `wl_output.scale` cannot be used for the
/// conversion — COSMIC reports 2 there regardless of the real factor — so the
/// region's own scale is the only correct source.
fn logical(x: i32, y: i32, output: usize) -> Result<(f64, f64)> {
    let out = outputs()?
        .get(output)
        .with_context(|| format!("no output #{output}"))?;
    let session = eis::session()?;
    let regions = session.regions();
    let region = regions
        .iter()
        .find(|region| region.physical_size() == (out.width, out.height))
        .with_context(|| {
            format!(
                "no EIS region matches output #{output} ({}x{}); regions: {regions:?}",
                out.width, out.height
            )
        })?;
    Ok((
        region.x + f64::from(x) / region.scale,
        region.y + f64::from(y) / region.scale,
    ))
}

fn outputs() -> Result<&'static [capture::OutputInfo]> {
    if OUTPUTS.get().is_none() {
        let list = capture::outputs().context("enumerate outputs")?;
        let _ = OUTPUTS.set(list);
    }
    Ok(OUTPUTS.get().map(Vec::as_slice).unwrap_or(&[]))
}

fn uinput_device() -> Result<MutexGuard<'static, Device>> {
    if UINPUT.get().is_none() {
        let main = outputs()?
            .first()
            .context("no output to map coordinates to")?;
        let device = Device::open("cosmic-desktop-mcp", main.width, main.height)?;
        let _ = UINPUT.set(Mutex::new(device));
    }
    UINPUT
        .get()
        .context("uinput device init raced")?
        .lock()
        .map_err(|err| anyhow::anyhow!("uinput device lock poisoned: {err}"))
}
