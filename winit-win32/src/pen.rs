//! The events of a pen, from the flags of consecutive pointer messages.

use winit_core::event::{ElementState, TabletToolButton, TabletToolKind};

/// `POINTER_FLAG_INRANGE`.
const IN_RANGE: u32 = 0x2;
/// `POINTER_FLAG_INCONTACT`.
const IN_CONTACT: u32 = 0x4;
/// `PEN_FLAG_BARREL`.
const BARREL: u32 = 0x1;
/// `PEN_FLAG_INVERTED`: the eraser end is in range.
const INVERTED: u32 = 0x2;
/// `PEN_FLAG_ERASER`: the eraser end touches.
const ERASER: u32 = 0x4;

/// What a pen reported in one pointer message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PenSnapshot {
    pub in_range: bool,
    pub contact: bool,
    pub barrel: bool,
    pub eraser: bool,
}

impl PenSnapshot {
    /// The snapshot of `pointerFlags` and `penFlags`. `leaving` is set for
    /// `WM_POINTERLEAVE`.
    pub(crate) fn of(pointer_flags: u32, pen_flags: u32, leaving: bool) -> Self {
        let in_range = !leaving && pointer_flags & (IN_RANGE | IN_CONTACT) != 0;
        if !in_range {
            return Self::default();
        }

        Self {
            in_range,
            contact: pointer_flags & IN_CONTACT != 0,
            barrel: pen_flags & BARREL != 0,
            eraser: pen_flags & (INVERTED | ERASER) != 0,
        }
    }

    fn kind(self) -> TabletToolKind {
        if self.eraser { TabletToolKind::Eraser } else { TabletToolKind::Pen }
    }
}

/// One event a change of the pen produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PenChange {
    Entered(TabletToolKind),
    Moved(TabletToolKind),
    Button(TabletToolKind, TabletToolButton, ElementState),
    Left(TabletToolKind),
}

/// The events from `before` to `now`, in order: a pen that leaves or turns
/// round releases its buttons and leaves first, one that arrives enters,
/// then it moves, then the barrel and the contact change.
pub(crate) fn changes(before: PenSnapshot, now: PenSnapshot) -> Vec<PenChange> {
    let mut events = Vec::new();
    let turned = before.in_range && now.in_range && before.eraser != now.eraser;

    if before.in_range && (!now.in_range || turned) {
        let kind = before.kind();
        if before.barrel {
            events.push(PenChange::Button(kind, TabletToolButton::Barrel, ElementState::Released));
        }
        if before.contact {
            events.push(PenChange::Button(kind, TabletToolButton::Contact, ElementState::Released));
        }
        events.push(PenChange::Left(kind));
    }

    if !now.in_range {
        return events;
    }

    let kind = now.kind();
    let from = if before.in_range && !turned { before } else { PenSnapshot::default() };
    if !from.in_range {
        events.push(PenChange::Entered(kind));
    }

    events.push(PenChange::Moved(kind));

    if from.barrel != now.barrel {
        events.push(PenChange::Button(kind, TabletToolButton::Barrel, state(now.barrel)));
    }

    if from.contact != now.contact {
        events.push(PenChange::Button(kind, TabletToolButton::Contact, state(now.contact)));
    }

    events
}

fn state(pressed: bool) -> ElementState {
    if pressed { ElementState::Pressed } else { ElementState::Released }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(flags: &[(u32, u32, bool)]) -> Vec<PenChange> {
        let mut before = PenSnapshot::default();
        let mut all = Vec::new();
        for &(pointer, pen, leaving) in flags {
            let now = PenSnapshot::of(pointer, pen, leaving);
            all.extend(changes(before, now));
            before = now;
        }
        all
    }

    const PEN: TabletToolKind = TabletToolKind::Pen;

    #[test]
    fn a_stroke_enters_touches_lifts_and_leaves_only_out_of_range() {
        let events = run(&[
            (IN_RANGE, 0, false),
            (IN_RANGE | IN_CONTACT, 0, false),
            (IN_RANGE | IN_CONTACT, 0, false),
            (IN_RANGE, 0, false),
            (0, 0, true),
        ]);
        assert_eq!(events, vec![
            PenChange::Entered(PEN),
            PenChange::Moved(PEN),
            PenChange::Moved(PEN),
            PenChange::Button(PEN, TabletToolButton::Contact, ElementState::Pressed),
            PenChange::Moved(PEN),
            PenChange::Moved(PEN),
            PenChange::Button(PEN, TabletToolButton::Contact, ElementState::Released),
            PenChange::Left(PEN),
        ]);
    }

    #[test]
    fn the_barrel_is_its_own_button_beside_the_contact() {
        let events = run(&[(IN_RANGE, BARREL, false), (IN_RANGE | IN_CONTACT, BARREL, false)]);
        assert!(events.contains(&PenChange::Button(
            PEN,
            TabletToolButton::Barrel,
            ElementState::Pressed
        )));
        assert_eq!(
            events.last(),
            Some(&PenChange::Button(PEN, TabletToolButton::Contact, ElementState::Pressed))
        );
    }

    #[test]
    fn the_inverted_pen_is_an_eraser_and_turning_it_leaves_first() {
        let events = run(&[(IN_RANGE, 0, false), (IN_RANGE, INVERTED, false)]);
        assert_eq!(
            events.get(2..),
            Some(
                &[
                    PenChange::Left(PEN),
                    PenChange::Entered(TabletToolKind::Eraser),
                    PenChange::Moved(TabletToolKind::Eraser),
                ][..]
            )
        );
        let touching = PenSnapshot::of(IN_RANGE | IN_CONTACT, ERASER, false);
        assert!(touching.eraser && touching.contact);
    }

    #[test]
    fn a_pen_that_vanishes_while_touching_releases_before_it_leaves() {
        let events = run(&[(IN_RANGE | IN_CONTACT, BARREL, false), (0, 0, false)]);
        assert_eq!(
            events.get(5..),
            Some(
                &[
                    PenChange::Button(PEN, TabletToolButton::Barrel, ElementState::Released),
                    PenChange::Button(PEN, TabletToolButton::Contact, ElementState::Released),
                    PenChange::Left(PEN),
                ][..]
            )
        );
    }
}
