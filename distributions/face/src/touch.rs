// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Touch into intent: one finger's samples, in the layout's frame, become
//! a tap on a button or a slide along a slider.
//!
//! A tap is a press and a lift within [`TAP_MS`] that travelled less than
//! [`TAP_TRAVEL`]; it names what was under the finger when it arrived.
//! A slide starts on a slider and that slider holds the finger until it
//! lifts: every sample is read along the slider's axis wherever the
//! finger has drifted, reported live ([`Intent::Slide`]) and once more
//! on the lift ([`Intent::Commit`]): the volume follows the finger, the
//! seek is applied when it lets go.
//! A press on a list that travels further than a tap up or down is a
//! scroll, reported as the finger's vertical travel since the last
//! sample ([`Intent::Scroll`]); its lift is no tap.

use embedded_graphics::prelude::Point;

use crate::layout::Hotspot;

/// The longest press that is still a tap, milliseconds.
pub const TAP_MS: u64 = 800;
/// The furthest a tap may travel, pixels.
pub const TAP_TRAVEL: i32 = 12;

/// What the finger meant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Intent {
    /// A press and lift on something.
    Tap(Hotspot),
    /// The finger on a slider, here, still down.
    Slide(Hotspot),
    /// The finger left a slider, here.
    Commit(Hotspot),
    /// The finger moved on a list: pixels down since the last sample
    /// (negative is up).
    Scroll(i32),
}

#[derive(Debug, Clone, Copy)]
struct Down {
    at_ms: u64,
    start: Point,
    /// What was under the finger when it arrived.
    hotspot: Option<Hotspot>,
    /// The slider's last fraction, while on one.
    fraction: Option<f32>,
    /// Where the finger was at the last sample.
    last: Point,
    /// The press became a scroll; the lift is no tap.
    scrolling: bool,
}

/// One finger's state between samples.
#[derive(Debug, Default)]
pub struct Tracker {
    down: Option<Down>,
}

impl Tracker {
    /// True while a finger is down, so the caller keeps reading the
    /// controller until the lift arrives.
    pub fn down(&self) -> bool {
        self.down.is_some()
    }

    /// The next sample: whether a finger is down, where (in the layout's
    /// frame, from [`crate::Layout::map_touch`]), and when. `hit` says
    /// what is under a point on the screen in front ([`crate::Face::hit`]);
    /// `along` reads a slider already held at any point
    /// ([`crate::Face::along`]).
    pub fn sample(
        &mut self,
        hit: impl Fn(Point) -> Option<Hotspot>,
        along: impl Fn(&Hotspot, Point) -> Option<f32>,
        touching: bool,
        p: Point,
        now_ms: u64,
    ) -> Option<Intent> {
        match (self.down, touching) {
            (None, false) => None,
            (None, true) => {
                let hotspot = hit(p);
                self.down = Some(Down {
                    at_ms: now_ms,
                    start: p,
                    hotspot,
                    fraction: slider_fraction(hotspot),
                    last: p,
                    scrolling: false,
                });
                None
            }
            (Some(mut d), true) => {
                let dy = p.y - d.last.y;
                d.last = p;
                if d.hotspot.is_some_and(|h| is_list(&h)) {
                    if !d.scrolling && (p.y - d.start.y).abs() > TAP_TRAVEL {
                        d.scrolling = true;
                        // The travel so far counts, not only this step.
                        let dy = p.y - d.start.y;
                        self.down = Some(d);
                        return Some(Intent::Scroll(dy));
                    }
                    self.down = Some(d);
                    return (d.scrolling && dy != 0)
                        .then_some(Intent::Scroll(dy));
                }
                let Some(kind) = d.hotspot.filter(is_slider) else {
                    self.down = Some(d);
                    return None;
                };
                let f = along(&kind, p).or(d.fraction)?;
                let moved =
                    d.fraction.is_none_or(|was| (was - f).abs() >= 0.005);
                d.fraction = Some(f);
                self.down = Some(d);
                moved.then(|| Intent::Slide(with_fraction(kind, f)))
            }
            (Some(d), false) => {
                self.down = None;
                let hotspot = d.hotspot?;
                if is_slider(&hotspot) {
                    return d
                        .fraction
                        .map(|f| Intent::Commit(with_fraction(hotspot, f)));
                }
                if d.scrolling {
                    return None;
                }
                let travel =
                    (p.x - d.start.x).abs().max((p.y - d.start.y).abs());
                (now_ms.saturating_sub(d.at_ms) <= TAP_MS
                    && travel <= TAP_TRAVEL)
                    .then_some(Intent::Tap(hotspot))
            }
        }
    }
}

fn is_list(h: &Hotspot) -> bool {
    matches!(h, Hotspot::Row(_) | Hotspot::List)
}

fn is_slider(h: &Hotspot) -> bool {
    matches!(h, Hotspot::Seek(_) | Hotspot::Volume(_))
}

fn slider_fraction(h: Option<Hotspot>) -> Option<f32> {
    match h? {
        Hotspot::Seek(f) | Hotspot::Volume(f) => Some(f),
        _ => None,
    }
}

fn with_fraction(kind: Hotspot, f: f32) -> Hotspot {
    match kind {
        Hotspot::Seek(_) => Hotspot::Seek(f),
        Hotspot::Volume(_) => Hotspot::Volume(f),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Layout, Screen, Style};

    fn layout() -> Layout {
        Layout::new(0, Style::Disc)
    }

    /// No slider on a page: the tracker never asks.
    fn no_slider(_: &Hotspot, _: Point) -> Option<f32> {
        None
    }

    #[test]
    fn a_drag_on_the_list_scrolls_and_its_lift_is_no_tap() {
        let l = layout();
        // Five rows filled of the seven the page holds.
        let hit = |p| l.hit_on(Screen::Library, 5, p);
        let mut t = Tracker::default();
        let row = l.page.row_rect(2).center();
        assert_eq!(t.sample(hit, no_slider, true, row, 0), None);
        // Within a tap's travel: nothing yet.
        assert_eq!(
            t.sample(hit, no_slider, true, row + Point::new(0, 5), 20),
            None
        );
        // Past it: a scroll of the whole travel so far, then the steps.
        assert_eq!(
            t.sample(hit, no_slider, true, row + Point::new(0, -20), 40),
            Some(Intent::Scroll(-20))
        );
        assert_eq!(
            t.sample(hit, no_slider, true, row + Point::new(0, -31), 60),
            Some(Intent::Scroll(-11))
        );
        assert_eq!(
            t.sample(hit, no_slider, false, row + Point::new(0, -31), 80),
            None
        );
        // A plain tap on a row names it.
        assert_eq!(t.sample(hit, no_slider, true, row, 100), None);
        assert_eq!(
            t.sample(hit, no_slider, false, row, 150),
            Some(Intent::Tap(Hotspot::Row(2)))
        );
        // Below the filled rows: the list, which a tap does nothing with.
        let empty = l.page.row_rect(6).center();
        t.sample(hit, no_slider, true, empty, 200);
        assert_eq!(
            t.sample(hit, no_slider, false, empty, 250),
            Some(Intent::Tap(Hotspot::List))
        );
    }

    #[test]
    fn a_short_still_press_on_play_is_a_tap() {
        let l = layout();
        let (hit, along) = (|p| l.hit(p), |h: &Hotspot, p| l.along(h, p));
        let mut t = Tracker::default();
        let play = l.transport.play;
        assert_eq!(t.sample(hit, along, true, play, 0), None);
        assert!(t.down());
        assert_eq!(
            t.sample(hit, along, true, play + Point::new(3, -2), 100),
            None
        );
        assert_eq!(
            t.sample(hit, along, false, play + Point::new(3, -2), 200),
            Some(Intent::Tap(Hotspot::Play))
        );
        assert!(!t.down());
    }

    #[test]
    fn a_long_press_or_a_far_travel_is_not_a_tap() {
        let l = layout();
        let (hit, along) = (|p| l.hit(p), |h: &Hotspot, p| l.along(h, p));
        let mut t = Tracker::default();
        let play = l.transport.play;
        t.sample(hit, along, true, play, 0);
        assert_eq!(t.sample(hit, along, false, play, TAP_MS + 1), None);
        t.sample(hit, along, true, play, 0);
        assert_eq!(
            t.sample(
                hit,
                along,
                false,
                play + Point::new(TAP_TRAVEL + 1, 0),
                50
            ),
            None
        );
    }

    #[test]
    fn a_finger_along_the_volume_slides_live_and_commits_on_lift() {
        let l = layout();
        let (hit, along) = (|p| l.hit(p), |h: &Hotspot, p| l.along(h, p));
        let mut t = Tracker::default();
        let bar = l.volume.bar;
        let y = bar.center().y;
        let at = |frac: f32| {
            Point::new(
                bar.top_left.x + (bar.size.width as f32 * frac) as i32,
                y,
            )
        };
        assert_eq!(t.sample(hit, along, true, at(0.25), 0), None);
        let Some(Intent::Slide(Hotspot::Volume(f))) =
            t.sample(hit, along, true, at(0.5), 40)
        else {
            panic!("a slide")
        };
        assert!((f - 0.5).abs() < 0.02, "{f}");
        // Far off the bar, over the text: the slider holds the finger and
        // reads it along the bar.
        let Some(Intent::Slide(Hotspot::Volume(f))) =
            t.sample(hit, along, true, Point::new(at(0.75).x, y - 60), 80)
        else {
            panic!("a slide off the bar")
        };
        assert!((f - 0.75).abs() < 0.02, "{f}");
        // Still, no new sample.
        assert_eq!(
            t.sample(hit, along, true, Point::new(at(0.75).x, y - 60), 100),
            None
        );
        let Some(Intent::Commit(Hotspot::Volume(f))) =
            t.sample(hit, along, false, Point::new(at(0.75).x, y - 60), 120)
        else {
            panic!("a commit")
        };
        assert!((f - 0.75).abs() < 0.02);
    }

    #[test]
    fn a_fader_holds_a_finger_that_wanders_and_reaches_its_ends() {
        let l = Layout::new(0, Style::Faders);
        let (hit, along) = (|p| l.hit(p), |h: &Hotspot, p| l.along(h, p));
        let mut t = Tracker::default();
        let bar = l.volume.bar;
        let (x, top, h) = (bar.center().x, bar.top_left.y, bar.size.height);
        let at = |frac: f32, dx: i32| {
            Point::new(x + dx, top + (h as f32 * (1.0 - frac)) as i32)
        };
        // Taken 18 px off the bar's centre: inside a fader's grab zone.
        assert_eq!(t.sample(hit, along, true, at(0.3, -18), 0), None);
        // Drifting 40 px into the text column, the fraction still reads.
        let Some(Intent::Slide(Hotspot::Volume(f))) =
            t.sample(hit, along, true, at(0.6, -40), 50)
        else {
            panic!("a slide while wandering")
        };
        assert!((f - 0.6).abs() < 0.02, "{f}");
        // Past the head, over the icon: full.
        let Some(Intent::Slide(Hotspot::Volume(f))) =
            t.sample(hit, along, true, Point::new(x, top - 30), 100)
        else {
            panic!("a slide past the head")
        };
        assert_eq!(f, 1.0);
        // Past the foot, over the label: none.
        let Some(Intent::Slide(Hotspot::Volume(f))) =
            t.sample(hit, along, true, Point::new(x, top + h as i32 + 30), 150)
        else {
            panic!("a slide past the foot")
        };
        assert_eq!(f, 0.0);
        // Fifty px off is the art: the fader does not take that finger.
        let mut fresh = Tracker::default();
        fresh.sample(hit, along, true, at(0.5, -50), 0);
        assert_eq!(
            fresh.sample(hit, along, false, at(0.5, -50), 50),
            Some(Intent::Tap(Hotspot::Art))
        );
    }

    #[test]
    fn the_ring_seeks_in_turns() {
        let l = layout();
        let (hit, along) = (|p| l.hit(p), |h: &Hotspot, p| l.along(h, p));
        let (Some((inner, outer)), c) = (l.ring_radii(), l.art_centre()) else {
            panic!("the disc has a ring")
        };
        let r = (inner + outer) / 2;
        let mut t = Tracker::default();
        // Twelve o'clock, then three o'clock: a quarter turn.
        t.sample(hit, along, true, c + Point::new(0, -r), 0);
        let Some(Intent::Slide(Hotspot::Seek(f))) =
            t.sample(hit, along, true, c + Point::new(r, 0), 50)
        else {
            panic!("a slide on the ring")
        };
        assert!((f - 0.25).abs() < 0.02, "{f}");
        // Six o'clock, but far outside the ring: the bearing still reads.
        let Some(Intent::Slide(Hotspot::Seek(f))) =
            t.sample(hit, along, true, c + Point::new(0, 2 * r), 75)
        else {
            panic!("a slide off the ring")
        };
        assert!((f - 0.5).abs() < 0.02, "{f}");
        let Some(Intent::Commit(Hotspot::Seek(f))) =
            t.sample(hit, along, false, c + Point::new(0, 2 * r), 100)
        else {
            panic!("a commit")
        };
        assert!((f - 0.5).abs() < 0.02);
    }

    #[test]
    fn touch_points_follow_the_rotation() {
        // The controller's (10, 20) in its portrait frame.
        assert_eq!(
            Layout::new(0, Style::Disc).map_touch(10, 20),
            Point::new(10, 20)
        );
        assert_eq!(
            Layout::new(90, Style::Disc).map_touch(10, 20),
            Point::new(20, 229)
        );
        assert_eq!(
            Layout::new(180, Style::Disc).map_touch(10, 20),
            Point::new(229, 299)
        );
        assert_eq!(
            Layout::new(270, Style::Disc).map_touch(10, 20),
            Point::new(299, 10)
        );
    }
}
