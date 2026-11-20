// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3999 — the screen a consent question opens on, on a Mac with several
//! screens: the one that holds the pointer when the question is shown, else
//! the primary screen, the menu bar's. The daemon's own panel
//! (`agents/roomlerd/src/indicator/mac.rs`) and the app's prompt
//! (`agents/roomler-desktop/src/panels.rs`) both ask [`choose`]. While the
//! pointer stays on one screen, both open there, and the prompt keeps clear
//! of the panel on it (#3994).
//!
//! Until #3999 the two chose apart. The panel opened on
//! `NSScreen::mainScreen`, the screen of the window with the keyboard focus,
//! and the prompt on the primary monitor. With that window on a screen
//! without the menu bar, two questions opened on two screens.
//!
//! Plain numbers, no AppKit types. Each caller reads `NSEvent::mouseLocation`
//! and `NSScreen::screens` on its main thread and passes them in AppKit's
//! global coordinates: points, the origin at the primary screen's
//! bottom-left corner, y up. Not Tauri's `cursor_position` and monitors:
//! across screens of different scale, their pixels are not AppKit's points.

/// A point in AppKit's global coordinates (`NSEvent::mouseLocation`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// A screen's frame in AppKit's global coordinates (`NSScreen::frame`): its
/// bottom-left corner, its width and its height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Frame {
    /// Whether the pointer is on this screen. Each axis is half-open, so a
    /// pointer on an edge two screens share is on exactly one of them:
    /// `x <= p.x < x + w`, and `y < p.y <= y + h`. y is the other way round
    /// because AppKit counts it up from the bottom: the pointer on a screen's
    /// top row reads `y + h`, and on its bottom row just above `y`. These are
    /// `NSMouseInRect`'s edges, not flipped. A coordinate that is not a number
    /// is on no screen.
    pub fn holds(&self, p: Point) -> bool {
        (self.x..self.x + self.w).contains(&p.x) && self.y < p.y && p.y <= self.y + self.h
    }
}

/// The screen a consent surface opens on: the first screen that holds the
/// pointer, else index 0, the primary, which `NSScreen::screens` lists first
/// (the menu bar's, at the origin). A pointer on no screen (a gap in the
/// layout, or a reading that is not a number) opens on the primary. `None`
/// only with no screens.
pub fn choose(pointer: Point, screens: &[Frame]) -> Option<usize> {
    if screens.is_empty() {
        return None;
    }
    Some(screens.iter().position(|s| s.holds(pointer)).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The menu bar's screen: AppKit puts its origin at (0, 0).
    const PRIMARY: Frame = Frame {
        x: 0.0,
        y: 0.0,
        w: 1440.0,
        h: 900.0,
    };

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// How many of the screens hold the pointer.
    fn holding(p: Point, screens: &[Frame]) -> usize {
        screens.iter().filter(|s| s.holds(p)).count()
    }

    #[test]
    fn two_screens_side_by_side_the_pointer_on_the_secondary_chooses_the_secondary() {
        let right = Frame {
            x: 1440.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        };
        assert_eq!(choose(at(2400.0, 500.0), &[PRIMARY, right]), Some(1));
        // A secondary on the left, where AppKit's x is below 0.
        let left = Frame {
            x: -1920.0,
            ..right
        };
        assert_eq!(choose(at(-500.0, 500.0), &[PRIMARY, left]), Some(1));
        // The pointer back on the primary.
        assert_eq!(choose(at(700.0, 450.0), &[PRIMARY, right]), Some(0));
    }

    #[test]
    fn a_pointer_on_the_edge_two_screens_share_is_on_exactly_one_of_them() {
        let right = Frame {
            x: 1440.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        };
        let screens = [PRIMARY, right];
        // x equal to the primary's right edge, which is the secondary's left.
        let edge = at(PRIMARY.x + PRIMARY.w, 450.0);
        assert_eq!(holding(edge, &screens), 1);
        assert_eq!(choose(edge, &screens), Some(1));
        // A hair to the left of it is the primary.
        assert_eq!(choose(at(PRIMARY.w - 0.5, 450.0), &screens), Some(0));
    }

    #[test]
    fn a_pointer_on_no_screen_opens_on_the_primary() {
        // A taller secondary on the right leaves a gap above the primary.
        let tall = Frame {
            x: 1440.0,
            y: 0.0,
            w: 1920.0,
            h: 1200.0,
        };
        let screens = [PRIMARY, tall];
        let gap = at(700.0, 1000.0);
        assert_eq!(holding(gap, &screens), 0);
        assert_eq!(choose(gap, &screens), Some(0));
        // Far outside the layout, and a reading that is not a number.
        assert_eq!(choose(at(-10_000.0, 50_000.0), &screens), Some(0));
        assert_eq!(choose(at(f64::NAN, 500.0), &screens), Some(0));
        assert_eq!(choose(at(2400.0, f64::INFINITY), &screens), Some(0));
    }

    #[test]
    fn with_one_screen_it_is_chosen_wherever_the_pointer_is() {
        for p in [
            at(700.0, 450.0),
            at(-1.0, -1.0),
            at(5000.0, 5000.0),
            at(f64::NAN, f64::NAN),
        ] {
            assert_eq!(choose(p, &[PRIMARY]), Some(0), "{p:?}");
        }
    }

    #[test]
    fn with_no_screens_none_is_chosen() {
        assert_eq!(choose(at(700.0, 450.0), &[]), None);
    }

    /// AppKit's y counts up from the primary's bottom edge, so a screen above
    /// the primary has a y above the primary's height, and one below it a y
    /// below 0. A caller that passed a flipped y would choose the wrong one.
    #[test]
    fn a_secondary_above_the_primary_is_chosen_with_the_pointer_on_it() {
        let above = Frame {
            x: -240.0,
            y: 900.0,
            w: 1920.0,
            h: 1080.0,
        };
        assert_eq!(choose(at(500.0, 1400.0), &[PRIMARY, above]), Some(1));
        assert_eq!(choose(at(500.0, 400.0), &[PRIMARY, above]), Some(0));
        let below = Frame {
            y: -1080.0,
            ..above
        };
        assert_eq!(choose(at(500.0, -400.0), &[PRIMARY, below]), Some(1));
        assert_eq!(choose(at(500.0, 400.0), &[PRIMARY, below]), Some(0));
    }

    /// The pointer on a screen's top row reads y = y + h, and on its bottom
    /// row just above y: AppKit counts y up. So a screen holds its top edge
    /// and not its bottom one (`NSMouseInRect`, not flipped), and the edge
    /// two stacked screens share is the lower screen's top row.
    #[test]
    fn a_screens_top_row_is_on_it_and_the_edge_it_shares_with_a_screen_above_is_its_own() {
        // Pushed against the top of the menu bar.
        let top_row = at(700.0, PRIMARY.y + PRIMARY.h);
        assert!(PRIMARY.holds(top_row));
        assert!(PRIMARY.holds(at(700.0, 1.0)), "the bottom row");
        assert!(!PRIMARY.holds(at(700.0, 0.0)), "the bottom edge");
        let above = Frame {
            x: 0.0,
            y: 900.0,
            w: 1920.0,
            h: 1080.0,
        };
        let screens = [PRIMARY, above];
        assert_eq!(holding(top_row, &screens), 1);
        assert_eq!(choose(top_row, &screens), Some(0));
        // The secondary's own top row is its own, not "no screen".
        let its_top = at(700.0, above.y + above.h);
        assert_eq!(holding(its_top, &screens), 1);
        assert_eq!(choose(its_top, &screens), Some(1));
        // A secondary on the right: the pointer on its top row.
        let right = Frame {
            x: 1440.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        };
        assert_eq!(choose(at(2400.0, 1080.0), &[PRIMARY, right]), Some(1));
    }
}
