//! Input over EIS (libei).
//!
//! There is one backend, and it goes through the RemoteDesktop portal: no
//! device permissions, logical coordinates from the compositor, works across
//! outputs with mixed scale factors. The cost is one authorization dialog per
//! machine (remembered afterwards) and one behavioural quirk — **a click does
//! not move keyboard focus on COSMIC**, so `click` followed by `type_text`
//! only lands where focus already was.
//!
//! Coordinates entering this module are pixels of a screenshot; EIS speaks
//! logical desktop coordinates, and `logical()` converts between them.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::eis;
use crate::keymap::{self, Button};
use crate::wl::capture;

static OUTPUTS: OnceLock<Vec<capture::OutputInfo>> = OnceLock::new();

pub fn describe() -> Result<String> {
    Ok(format!(
        "backend: eis (libei through the RemoteDesktop portal)\n{}",
        eis::session()?.describe()
    ))
}

/// Moves the pointer. `x`/`y` are pixels in the screenshot of `output`.
pub fn move_to(x: i32, y: i32, output: usize) -> Result<()> {
    let (lx, ly) = logical(x, y, output)?;
    eis::session()?.move_to(lx, ly)
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

    // EIS scroll axes follow the touchpad convention: positive y is down, while
    // the tool's dy is in wheel clicks with positive = up (the kernel REL_WHEEL
    // convention). Hence the negation.
    eis::session()?.scroll_clicks(dx, -dy)
}

pub fn key(code: u16, pressed: bool) -> Result<()> {
    eis::session()?.key(u32::from(code), pressed)
}

/// Presses a `ctrl+s` style chord: modifiers in the given order, released in
/// reverse.
pub fn chord(combo: &str) -> Result<()> {
    let codes = keymap::parse_chord(combo)?;
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
            keymap::keycode_for(ch).with_context(|| format!("character {ch:?} needs an input method"))?;
        if shift {
            key(keymap::KEY_LEFTSHIFT, true)?;
        }
        key(code, true)?;
        key(code, false)?;
        if shift {
            key(keymap::KEY_LEFTSHIFT, false)?;
        }
    }
    Ok(())
}

fn set_button(button: Button, pressed: bool) -> Result<()> {
    eis::session()?.button(u32::from(button.code()), pressed)
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
