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

use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::eis;
use crate::keymap::{self, Button};
use crate::wl::capture;

pub fn describe() -> Result<String> {
    Ok(format!(
        "backend: eis (libei through the RemoteDesktop portal)\n{}",
        eis::session()?.describe()
    ))
}

/// Moves the pointer to a logical desktop coordinate.
///
/// The tool layer resolves either an output screenshot pixel or a window image
/// pixel into this space first; keeping the EIS calls in logical coordinates
/// means there is exactly one place where the conversion can be wrong.
pub fn move_logical(x: f64, y: f64) -> Result<()> {
    eis::session()?.move_to(x, y)
}

pub fn click_logical(button: Button, lx: f64, ly: f64, count: u32) -> Result<()> {
    move_logical(lx, ly)?;
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

pub fn scroll_logical(lx: f64, ly: f64, dx: i32, dy: i32) -> Result<()> {
    move_logical(lx, ly)?;
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
        let (code, shift) = keymap::keycode_for(ch)
            .with_context(|| format!("character {ch:?} needs an input method"))?;
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

/// An EIS region and how it was tied to a `wl_output`.
#[derive(Debug)]
pub struct RegionMatch {
    pub region: eis::Region,
    /// `"name"` when `wl_output.name` met the region's `mapping_id`, `"size"`
    /// when it had to be inferred from the physical resolution.
    pub by: &'static str,
}

/// Finds the EIS region that corresponds to `out`.
///
/// Name first: `wl_output.name` and the region's `mapping_id` are the same
/// connector string (`DP-2`, `eDP-1`). Only when a side lacks a name does this
/// fall back to matching physical size, and then only if exactly one region
/// has that size. The old unconditional `find` picked the first of several
/// same-sized regions, which made `output: 1` clicks land silently on output 0
/// with two identical monitors.
pub fn region_for(out: &capture::OutputInfo, regions: &[eis::Region]) -> Result<RegionMatch> {
    if let Some(name) = out.name.as_deref() {
        let named: Vec<&eis::Region> = regions
            .iter()
            .filter(|region| region.mapping_id.as_deref() == Some(name))
            .collect();
        match named.len() {
            1 => {
                return Ok(RegionMatch {
                    region: named[0].clone(),
                    by: "name",
                });
            }
            0 => {}
            count => bail!(
                "output #{} ({name}) matches {count} EIS regions with the same connector name; \
                 cannot tell them apart",
                out.index
            ),
        }
    }

    let same_size: Vec<&eis::Region> = regions
        .iter()
        .filter(|region| region.physical_size() == (out.width, out.height))
        .collect();
    match same_size.len() {
        1 => Ok(RegionMatch {
            region: same_size[0].clone(),
            by: "size",
        }),
        0 => bail!(
            "no EIS region matches output #{} ({}x{}{})",
            out.index,
            out.width,
            out.height,
            out.name
                .as_deref()
                .map(|name| format!(" {name}"))
                .unwrap_or_default()
        ),
        count => bail!(
            "output #{} ({}) matches {count} EIS regions of the same physical size and cannot be \
             told apart; the compositor did not report a usable connector name",
            out.index,
            out.name.as_deref().unwrap_or("unnamed")
        ),
    }
}

/// Enumerates outputs right now.
///
/// Deliberately uncached: a cached list goes stale when a monitor is plugged
/// in, unplugged or rescaled, and the failure mode is clicking on the wrong
/// place. A fresh roundtrip costs about a millisecond.
pub fn outputs() -> Result<Vec<capture::OutputInfo>> {
    capture::outputs().context("enumerate outputs")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(name: &str, width: i32, height: i32, scale: f64) -> eis::Region {
        eis::Region {
            x: 0.0,
            y: 0.0,
            width,
            height,
            scale,
            mapping_id: Some(name.into()),
        }
    }

    fn output(name: Option<&str>, width: i32, height: i32) -> capture::OutputInfo {
        capture::OutputInfo {
            index: 0,
            name: name.map(Into::into),
            width,
            height,
            scale: 1,
        }
    }

    #[test]
    fn matching_name_wins_over_size() {
        let regions = vec![
            region("DP-1", 1920, 1080, 1.0),
            region("DP-2", 1920, 1080, 1.0),
        ];
        let matched = region_for(&output(Some("DP-2"), 1920, 1080), &regions).unwrap();
        assert_eq!(matched.by, "name");
        assert_eq!(matched.region.mapping_id.as_deref(), Some("DP-2"));
    }

    /// Two identical monitors and no names anywhere: the old `find` silently
    /// picked the first region, sending `output: 1` clicks to output 0. This
    /// must stay a loud error.
    #[test]
    fn identical_sizes_without_names_is_an_error_not_a_guess() {
        let regions = vec![
            region("DP-1", 1920, 1080, 1.0),
            region("DP-2", 1920, 1080, 1.0),
        ];
        assert!(region_for(&output(None, 1920, 1080), &regions).is_err());
    }

    #[test]
    fn unique_size_still_matches_when_names_are_missing() {
        // Region width/height are logical; DP-2 at 1.25 measures 2560x1440
        // physically, matching the output's physical resolution.
        let regions = vec![
            region("DP-1", 1920, 1080, 1.0),
            region("DP-2", 2048, 1152, 1.25),
        ];
        let matched = region_for(&output(None, 2560, 1440), &regions).unwrap();
        assert_eq!(matched.by, "size");
        assert_eq!(matched.region.mapping_id.as_deref(), Some("DP-2"));
    }

    #[test]
    fn duplicate_connector_names_are_an_error_not_the_first_match() {
        let regions = vec![
            region("DP-2", 1920, 1080, 1.0),
            region("DP-2", 1920, 1080, 1.0),
        ];

        let err = region_for(&output(Some("DP-2"), 1920, 1080), &regions)
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 EIS regions"), "{err}");
    }
}
