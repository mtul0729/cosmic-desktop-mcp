//! The three coordinate spaces the tools speak, and the conversions between
//! them.
//!
//! Mixing them is the most expensive mistake this server can make. The incident
//! that added this module: a window on output 1 had a crop origin of (966,60) in
//! that output's screenshot, the caller added it to a window-image point and
//! called `click`, which defaults to output 0. Every click landed on the other
//! monitor and the tool answered "clicked ... on output 0" with no hint that
//! anything was wrong.
//!
//! The spaces:
//!
//! 1. **logical** — the compositor's desktop coordinates. A window's `logical`
//!    rect is relative to its output; an EIS absolute pointer uses desktop-wide
//!    logical coordinates. No MCP tool accepts logical coordinates.
//! 2. **output screenshot pixels** — the PNG from `screenshot {output}`.
//!    `click` / `move` / `scroll` take these, together with the output index.
//! 3. **window image pixels** — the PNG from `screenshot_window {window}`, a
//!    crop of space 2 at the window's origin. `click_window` / `move_window` /
//!    `scroll_window` take these and resolve the output and crop origin
//!    themselves.
//!
//! A point moves between 2 and 3 by adding the crop origin; between 2 and 1 by
//! dividing by the output's real scale (the EIS region's scale, never
//! `wl_output.scale`, which COSMIC always reports as 2) and adding the region's
//! logical origin.

use anyhow::{Result, bail};

use crate::eis::Region;
use crate::wl::toplevel::Rect;

/// Multiplies a logical rect by `scale`.
///
/// Pure, so callers that must not open an EIS session (anything status-like)
/// can convert from a region they already have.
pub fn scale_rect(rect: Rect, scale: f64) -> Rect {
    let scaled = |value: i32| (f64::from(value) * scale).round() as i32;
    Rect {
        x: scaled(rect.x),
        y: scaled(rect.y),
        width: scaled(rect.width),
        height: scaled(rect.height),
    }
}

/// Half-open containment: a rect's right/bottom edge is not inside it.
pub fn rect_contains(rect: Rect, x: i32, y: i32) -> bool {
    x >= rect.x && y >= rect.y && x < rect.x + rect.width && y < rect.y + rect.height
}

/// A window's placement in all three spaces.
#[derive(Clone, Debug)]
pub struct WindowGeometry {
    /// The window's output, in logical coordinates and with the real scale.
    pub region: Region,
    pub output_index: usize,
    pub output_name: Option<String>,
    /// Placement as the compositor reported it: relative to the output.
    pub logical: Rect,
    /// The same rect in output screenshot pixels, before clamping.
    pub pixels: Rect,
    /// The part of `pixels` that actually fits on the output. This is the
    /// crop `screenshot_window` returns, and its origin is what a window-image
    /// point is offset by.
    pub image: Rect,
}

impl WindowGeometry {
    /// Returns `None` when the window is entirely outside the output, because
    /// there is no crop and no point inside it.
    pub fn new(
        region: Region,
        output_index: usize,
        output_name: Option<String>,
        logical: Rect,
        output_size: (i32, i32),
    ) -> Option<Self> {
        let pixels = scale_rect(logical, region.scale);
        let image = intersect(pixels, output_size)?;
        Some(Self {
            region,
            output_index,
            output_name,
            logical,
            pixels,
            image,
        })
    }

    /// Converts a point in the image `screenshot_window` returned into that
    /// output's screenshot pixels.
    pub fn window_to_output(&self, x: i32, y: i32) -> Result<(i32, i32)> {
        if x < 0 || y < 0 || x >= self.image.width || y >= self.image.height {
            bail!(
                "point ({x}, {y}) is outside the window image, which is {}x{}; take the point \
                 from a fresh screenshot_window — the window may have moved or been resized",
                self.image.width,
                self.image.height
            );
        }
        Ok((self.image.x + x, self.image.y + y))
    }

    /// Output screenshot pixels -> desktop logical coordinates.
    pub fn output_to_logical(&self, x: i32, y: i32) -> (f64, f64) {
        (
            self.region.x + f64::from(x) / self.region.scale,
            self.region.y + f64::from(y) / self.region.scale,
        )
    }

    /// Window image pixels -> desktop logical coordinates.
    pub fn window_to_logical(&self, x: i32, y: i32) -> Result<(f64, f64)> {
        let (ox, oy) = self.window_to_output(x, y)?;
        Ok(self.output_to_logical(ox, oy))
    }
}

fn intersect(rect: Rect, size: (i32, i32)) -> Option<Rect> {
    let left = rect.x.max(0);
    let top = rect.y.max(0);
    let right = (rect.x + rect.width).min(size.0);
    let bottom = (rect.y + rect.height).min(size.1);
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

/// One window that could be under a point, in one output's screenshot pixels.
#[derive(Clone, Debug)]
pub struct HitCandidate {
    /// Caller's index, returned verbatim so the result can be mapped back.
    pub index: usize,
    pub rect: Rect,
    pub activated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HitResult {
    /// The single window under the point, or the activated one when several
    /// overlap. `None` means either the desktop or an ambiguity.
    pub window: Option<usize>,
    /// Every window whose reported rect covers the point.
    pub candidates: Vec<usize>,
    /// Several rects cover the point. The protocol exposes no stacking order,
    /// so this is the caller's warning that `window` may be a guess.
    pub ambiguous: bool,
}

/// Geometry-only hit test.
///
/// The compositor does not expose a stacking order, and a window's geometry is
/// its last reported placement rather than what is visible, so this is the
/// strongest answer the protocol supports: every rect that covers the point,
/// plus the activated one as the best guess when several do.
pub fn hit_test(candidates: &[HitCandidate], x: i32, y: i32) -> HitResult {
    let hits: Vec<&HitCandidate> = candidates
        .iter()
        .filter(|candidate| rect_contains(candidate.rect, x, y))
        .collect();
    let indices: Vec<usize> = hits.iter().map(|candidate| candidate.index).collect();

    match hits.as_slice() {
        [] => HitResult::default(),
        [only] => HitResult {
            window: Some(only.index),
            candidates: indices,
            ambiguous: false,
        },
        many => {
            let activated: Vec<&&HitCandidate> = many
                .iter()
                .filter(|candidate| candidate.activated)
                .collect();
            let window = match activated.as_slice() {
                [only] => Some(only.index),
                _ => None,
            };
            HitResult {
                window,
                candidates: indices,
                ambiguous: true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact numbers from the incident: a 628x672 window at logical
    /// (644,40) on eDP-1, whose region is 1280x720 at (384,1152), scale 1.5.
    /// The crop is 942x1008 at (966,60), and window-image (468,84) is output
    /// pixel (1434,144) and logical (1340,1248).
    fn incident_geometry() -> WindowGeometry {
        WindowGeometry::new(
            Region {
                x: 384.0,
                y: 1152.0,
                width: 1280,
                height: 720,
                scale: 1.5,
                mapping_id: Some("eDP-1".into()),
            },
            1,
            Some("eDP-1".into()),
            Rect {
                x: 644,
                y: 40,
                width: 628,
                height: 672,
            },
            (1920, 1080),
        )
        .expect("the window is on the output")
    }

    #[test]
    fn logical_to_output_pixels_uses_the_region_scale() {
        let geometry = incident_geometry();
        assert_eq!(
            geometry.pixels,
            Rect {
                x: 966,
                y: 60,
                width: 942,
                height: 1008
            }
        );
        assert_eq!(geometry.image, geometry.pixels);
    }

    #[test]
    fn window_image_point_maps_through_output_pixels_to_logical() {
        let geometry = incident_geometry();
        assert_eq!(geometry.window_to_output(468, 84).unwrap(), (1434, 144));
        let (lx, ly) = geometry.window_to_logical(468, 84).unwrap();
        assert!((lx - 1340.0).abs() < 1e-9, "logical x was {lx}");
        assert!((ly - 1248.0).abs() < 1e-9, "logical y was {ly}");
    }

    #[test]
    fn a_point_outside_the_image_is_an_error_that_names_the_size() {
        let geometry = incident_geometry();
        let err = geometry.window_to_output(942, 0).unwrap_err().to_string();
        assert!(err.contains("942x1008"), "{err}");
        assert!(geometry.window_to_output(0, 1008).is_err());
        assert!(geometry.window_to_output(-1, 0).is_err());
    }

    #[test]
    fn a_window_partly_off_screen_is_clipped_to_the_visible_part() {
        let geometry = WindowGeometry::new(
            Region {
                x: 0.0,
                y: 0.0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                mapping_id: None,
            },
            0,
            None,
            Rect {
                x: -20,
                y: -10,
                width: 100,
                height: 50,
            },
            (1920, 1080),
        )
        .unwrap();
        assert_eq!(
            geometry.image,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 40
            }
        );
        // Image (0,0) is output (0,0), not logical (-20,-10).
        assert_eq!(geometry.window_to_output(0, 0).unwrap(), (0, 0));
    }

    #[test]
    fn a_window_entirely_off_screen_has_no_crop() {
        let geometry = WindowGeometry::new(
            Region {
                x: 0.0,
                y: 0.0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                mapping_id: None,
            },
            0,
            None,
            Rect {
                x: 2000,
                y: 0,
                width: 100,
                height: 100,
            },
            (1920, 1080),
        );
        assert!(geometry.is_none());
    }

    fn candidate(
        index: usize,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        activated: bool,
    ) -> HitCandidate {
        HitCandidate {
            index,
            rect: Rect {
                x,
                y,
                width,
                height,
            },
            activated,
        }
    }

    #[test]
    fn hit_test_finds_the_one_window_under_the_point() {
        let candidates = vec![
            candidate(0, 0, 0, 100, 100, false),
            candidate(1, 200, 200, 100, 100, false),
        ];
        let hit = hit_test(&candidates, 250, 250);
        assert_eq!(hit.window, Some(1));
        assert_eq!(hit.candidates, vec![1]);
        assert!(!hit.ambiguous);
    }

    #[test]
    fn hit_test_on_the_desktop_is_empty_not_an_error() {
        let candidates = vec![candidate(0, 0, 0, 100, 100, false)];
        let hit = hit_test(&candidates, 150, 150);
        assert_eq!(hit.window, None);
        assert!(hit.candidates.is_empty());
    }

    #[test]
    fn hit_test_uses_half_open_edges() {
        let candidates = vec![candidate(0, 10, 10, 100, 100, false)];
        assert_eq!(hit_test(&candidates, 10, 10).window, Some(0));
        assert_eq!(hit_test(&candidates, 109, 109).window, Some(0));
        assert_eq!(hit_test(&candidates, 110, 109).window, None);
        assert_eq!(hit_test(&candidates, 109, 110).window, None);
    }

    #[test]
    fn overlapping_windows_prefer_the_activated_one_and_report_the_ambiguity() {
        let candidates = vec![
            candidate(0, 0, 0, 100, 100, false),
            candidate(1, 50, 50, 100, 100, true),
        ];
        let hit = hit_test(&candidates, 75, 75);
        assert_eq!(hit.window, Some(1));
        assert_eq!(hit.candidates, vec![0, 1]);
        assert!(hit.ambiguous);
    }

    #[test]
    fn overlapping_windows_with_no_activated_one_do_not_guess() {
        let candidates = vec![
            candidate(0, 0, 0, 100, 100, false),
            candidate(1, 50, 50, 100, 100, false),
        ];
        let hit = hit_test(&candidates, 75, 75);
        assert_eq!(hit.window, None);
        assert_eq!(hit.candidates, vec![0, 1]);
        assert!(hit.ambiguous);
    }
}
