// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Touch into intent: one finger's samples, in the layout's frame, become
//! a tap on a button or a slide along a slider.
//!
//! A tap is a press and a lift within [`TAP_MS`] that travelled less than
//! [`TAP_TRAVEL`]; it names what was under the finger when it arrived.
//! A slide starts on a slider and follows the finger along that slider
//! even when it drifts off the bar, reporting the fraction live
//! ([`Intent::Slide`]) and once more on the lift ([`Intent::Commit`]):
//! the volume follows the finger, the seek is applied when it lets go.

use embedded_graphics::prelude::Point;

use crate::layout::{Hotspot, Layout};

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
}

#[derive(Debug, Clone, Copy)]
struct Down {
    at_ms: u64,
    start: Point,
    /// What was under the finger when it arrived.
    hotspot: Option<Hotspot>,
    /// The slider's last fraction, while on one.
    fraction: Option<f32>,
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
    /// frame, from [`Layout::map_touch`]), and when.
    pub fn sample(
        &mut self,
        layout: &Layout,
        touching: bool,
        p: Point,
        now_ms: u64,
    ) -> Option<Intent> {
        match (self.down, touching) {
            (None, false) => None,
            (None, true) => {
                let hotspot = layout.hit(p);
                self.down = Some(Down {
                    at_ms: now_ms,
                    start: p,
                    hotspot,
                    fraction: slider_fraction(hotspot),
                });
                None
            }
            (Some(mut d), true) => {
                let kind = d.hotspot.filter(is_slider)?;
                let now = layout.hit(p).filter(|h| same_slider(h, &kind));
                let f = slider_fraction(now).or(d.fraction)?;
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
                let travel =
                    (p.x - d.start.x).abs().max((p.y - d.start.y).abs());
                (now_ms.saturating_sub(d.at_ms) <= TAP_MS
                    && travel <= TAP_TRAVEL)
                    .then_some(Intent::Tap(hotspot))
            }
        }
    }
}

fn is_slider(h: &Hotspot) -> bool {
    matches!(h, Hotspot::Seek(_) | Hotspot::Volume(_))
}

fn same_slider(a: &Hotspot, b: &Hotspot) -> bool {
    matches!(
        (a, b),
        (Hotspot::Seek(_), Hotspot::Seek(_))
            | (Hotspot::Volume(_), Hotspot::Volume(_))
    )
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
    use crate::layout::Style;

    fn layout() -> Layout {
        Layout::new(0, Style::Disc)
    }

    #[test]
    fn a_short_still_press_on_play_is_a_tap() {
        let l = layout();
        let mut t = Tracker::default();
        let play = l.transport.play;
        assert_eq!(t.sample(&l, true, play, 0), None);
        assert!(t.down());
        assert_eq!(t.sample(&l, true, play + Point::new(3, -2), 100), None);
        assert_eq!(
            t.sample(&l, false, play + Point::new(3, -2), 200),
            Some(Intent::Tap(Hotspot::Play))
        );
        assert!(!t.down());
    }

    #[test]
    fn a_long_press_or_a_far_travel_is_not_a_tap() {
        let l = layout();
        let mut t = Tracker::default();
        let play = l.transport.play;
        t.sample(&l, true, play, 0);
        assert_eq!(t.sample(&l, false, play, TAP_MS + 1), None);
        t.sample(&l, true, play, 0);
        assert_eq!(
            t.sample(&l, false, play + Point::new(TAP_TRAVEL + 1, 0), 50),
            None
        );
    }

    #[test]
    fn a_finger_along_the_volume_slides_live_and_commits_on_lift() {
        let l = layout();
        let mut t = Tracker::default();
        let bar = l.volume.bar;
        let y = bar.center().y;
        let at = |frac: f32| {
            Point::new(
                bar.top_left.x + (bar.size.width as f32 * frac) as i32,
                y,
            )
        };
        assert_eq!(t.sample(&l, true, at(0.25), 0), None);
        let Some(Intent::Slide(Hotspot::Volume(f))) =
            t.sample(&l, true, at(0.5), 40)
        else {
            panic!("a slide")
        };
        assert!((f - 0.5).abs() < 0.02, "{f}");
        // Off the bar's touch zone: the last fraction holds, nothing new.
        assert_eq!(t.sample(&l, true, Point::new(at(0.5).x, y - 60), 80), None);
        let Some(Intent::Commit(Hotspot::Volume(f))) =
            t.sample(&l, false, at(0.5), 120)
        else {
            panic!("a commit")
        };
        assert!((f - 0.5).abs() < 0.02);
    }

    #[test]
    fn the_ring_seeks_in_turns() {
        let l = layout();
        let (Some((inner, outer)), c) = (l.ring_radii(), l.art_centre()) else {
            panic!("the disc has a ring")
        };
        let r = (inner + outer) / 2;
        let mut t = Tracker::default();
        // Twelve o'clock, then three o'clock: a quarter turn.
        t.sample(&l, true, c + Point::new(0, -r), 0);
        let Some(Intent::Slide(Hotspot::Seek(f))) =
            t.sample(&l, true, c + Point::new(r, 0), 50)
        else {
            panic!("a slide on the ring")
        };
        assert!((f - 0.25).abs() < 0.02, "{f}");
        let Some(Intent::Commit(Hotspot::Seek(f))) =
            t.sample(&l, false, c + Point::new(r, 0), 100)
        else {
            panic!("a commit")
        };
        assert!((f - 0.25).abs() < 0.02);
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
