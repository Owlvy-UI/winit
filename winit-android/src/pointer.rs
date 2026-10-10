//! Motion events of fingers, mice and styluses as winit pointer events.
//!
//! The event loop copies what it needs out of an Android `MotionEvent` into a [`Motion`] and
//! [`Pointers`] turns it into window events. A mouse or stylus that hovers keeps its pointer
//! across a press: Android ends the hover right before the press and starts it again right after
//! the release, so a hover exit only becomes [`WindowEvent::PointerLeft`] when the input batch
//! ends without a press of the same device.

use std::collections::BTreeMap;
use std::f64::consts::{FRAC_PI_2, TAU};

use dpi::PhysicalPosition;
use winit_core::event::{
    ButtonSource, DeviceId, ElementState, FingerId, Force, MouseButton, MouseScrollDelta,
    PointerKind, PointerSource, TabletToolAngle, TabletToolButton, TabletToolData, TabletToolKind,
    TouchPhase, WindowEvent,
};

/// `AMOTION_EVENT_BUTTON_PRIMARY`.
pub(crate) const BUTTON_PRIMARY: u32 = 1 << 0;
/// `AMOTION_EVENT_BUTTON_SECONDARY`.
pub(crate) const BUTTON_SECONDARY: u32 = 1 << 1;
/// `AMOTION_EVENT_BUTTON_TERTIARY`.
pub(crate) const BUTTON_TERTIARY: u32 = 1 << 2;
/// `AMOTION_EVENT_BUTTON_BACK`.
pub(crate) const BUTTON_BACK: u32 = 1 << 3;
/// `AMOTION_EVENT_BUTTON_FORWARD`.
pub(crate) const BUTTON_FORWARD: u32 = 1 << 4;
/// `AMOTION_EVENT_BUTTON_STYLUS_PRIMARY`.
pub(crate) const BUTTON_STYLUS_PRIMARY: u32 = 1 << 5;
/// `AMOTION_EVENT_BUTTON_STYLUS_SECONDARY`.
pub(crate) const BUTTON_STYLUS_SECONDARY: u32 = 1 << 6;

/// The mouse buttons in the order their changes are reported.
const MOUSE_BUTTONS: [(u32, MouseButton); 5] = [
    (BUTTON_PRIMARY, MouseButton::Left),
    (BUTTON_SECONDARY, MouseButton::Right),
    (BUTTON_TERTIARY, MouseButton::Middle),
    (BUTTON_BACK, MouseButton::Back),
    (BUTTON_FORWARD, MouseButton::Forward),
];

/// The stylus buttons in the order their changes are reported.
const STYLUS_BUTTONS: [(u32, TabletToolButton); 2] = [
    (BUTTON_STYLUS_PRIMARY, TabletToolButton::Barrel),
    (BUTTON_STYLUS_SECONDARY, TabletToolButton::Other(1)),
];

/// The most hovering devices tracked at once.
const MAX_DEVICES: usize = 16;

/// What a motion event reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The first or a further pointer went down; the acting pointer is named by index.
    Down(usize),
    /// The last or one of several pointers went up; the acting pointer is named by index.
    Up(usize),
    Move,
    Cancel,
    HoverEnter,
    HoverMove,
    HoverExit,
    /// A button changed without a contact change.
    Button,
    Scroll,
    Other,
}

/// The tool of one pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    Finger,
    Mouse,
    Stylus,
    Eraser,
    Unknown,
}

/// One pointer of a motion event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Contact {
    pub(crate) id: usize,
    pub(crate) tool: Tool,
    pub(crate) position: PhysicalPosition<f64>,
    pub(crate) pressure: f32,
    /// `AXIS_TILT` in radians from the perpendicular.
    pub(crate) tilt: f32,
    /// `AXIS_ORIENTATION` in radians, 0 pointing up, clockwise positive.
    pub(crate) orientation: f32,
    pub(crate) hscroll: f32,
    pub(crate) vscroll: f32,
}

/// The parts of one motion event the pointers need.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Motion {
    pub(crate) device: i64,
    pub(crate) phase: Phase,
    pub(crate) buttons: u32,
    pub(crate) contacts: Vec<Contact>,
}

/// Whether a mouse or stylus touches the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Press {
    Up,
    Down,
    /// Down without a reported button, standing for the left button.
    Emulated,
}

/// A mouse or stylus that may hover.
#[derive(Debug, Clone, Copy)]
struct Hovering {
    tool: Tool,
    inside: bool,
    leaving: bool,
    press: Press,
    buttons: u32,
    position: PhysicalPosition<f64>,
}

/// Pointer state of the window.
#[derive(Debug, Default)]
pub(crate) struct Pointers {
    primary_finger: Option<FingerId>,
    devices: BTreeMap<i64, Hovering>,
}

/// The tablet tool kind of a tool, for styluses and erasers.
fn tablet_kind(tool: Tool) -> Option<TabletToolKind> {
    match tool {
        Tool::Stylus => Some(TabletToolKind::Pen),
        Tool::Eraser => Some(TabletToolKind::Eraser),
        _ => None,
    }
}

/// The angle of a stylus from `AXIS_TILT` and `AXIS_ORIENTATION`.
fn angle(contact: &Contact) -> Option<TabletToolAngle> {
    let tilt = f64::from(contact.tilt);
    let orientation = f64::from(contact.orientation);
    if !tilt.is_finite() || !orientation.is_finite() {
        return None;
    }

    let altitude = (FRAC_PI_2 - tilt.abs()).clamp(0.0, FRAC_PI_2);
    let azimuth = if tilt.abs() > f64::EPSILON { (orientation - FRAC_PI_2).rem_euclid(TAU) } else { 0.0 };
    Some(TabletToolAngle { altitude, azimuth })
}

/// The force of a pressure reading.
fn force(pressure: f32) -> Force {
    Force::Normalized(f64::from(pressure))
}

/// The tablet tool data of a stylus contact.
fn tool_data(contact: &Contact) -> TabletToolData {
    TabletToolData { force: Some(force(contact.pressure)), angle: angle(contact), ..TabletToolData::default() }
}

impl Pointers {
    /// The events of one motion event.
    pub(crate) fn handle(&mut self, motion: &Motion) -> Vec<WindowEvent> {
        let mut events = Vec::new();
        if motion.phase == Phase::Scroll {
            for contact in &motion.contacts {
                let delta = MouseScrollDelta::LineDelta(-contact.hscroll, contact.vscroll);
                events.push(WindowEvent::MouseWheel {
                    device_id: Some(DeviceId::from_raw(motion.device)),
                    delta,
                    phase: TouchPhase::Moved,
                });
            }

            return events;
        }

        for (index, contact) in motion.contacts.iter().enumerate() {
            let acting = match motion.phase {
                Phase::Down(acting) | Phase::Up(acting) => acting == index,
                _ => true,
            };
            if !acting {
                continue;
            }

            match contact.tool {
                Tool::Finger | Tool::Unknown => self.touch(motion, contact, &mut events),
                Tool::Mouse | Tool::Stylus | Tool::Eraser => self.hover(motion, contact, &mut events),
            }
        }

        events
    }

    /// Ends the input batch: a hover exit without a following press leaves.
    pub(crate) fn finish(&mut self) -> Vec<WindowEvent> {
        let mut events = Vec::new();
        for (device, state) in &mut self.devices {
            if state.leaving {
                state.leaving = false;
                state.inside = false;
                events.push(WindowEvent::PointerLeft {
                    device_id: Some(DeviceId::from_raw(*device)),
                    primary: true,
                    position: Some(state.position),
                    kind: kind_of(state.tool),
                });
            }
        }

        self.devices.retain(|_, state| state.inside || state.press != Press::Up || state.buttons != 0);
        events
    }

    fn touch(&mut self, motion: &Motion, contact: &Contact, events: &mut Vec<WindowEvent>) {
        let device_id = Some(DeviceId::from_raw(motion.device));
        let finger_id = FingerId::from_raw(contact.id);
        let finger = contact.tool == Tool::Finger;
        let force = Some(force(contact.pressure));
        let position = contact.position;
        let kind = if finger { PointerKind::Touch(finger_id) } else { PointerKind::Unknown };
        match motion.phase {
            Phase::Down(_) => {
                let primary = self.primary_finger.is_none();
                if primary {
                    self.primary_finger = Some(finger_id);
                }

                events.push(WindowEvent::PointerEntered { device_id, primary, position, kind });
                let button = if finger {
                    ButtonSource::Touch { finger_id, force }
                } else {
                    ButtonSource::Unknown(0)
                };
                events.push(WindowEvent::PointerButton {
                    device_id,
                    primary,
                    state: ElementState::Pressed,
                    position,
                    button,
                    is_macos_activation_click: false,
                });
            },
            Phase::Move => {
                let primary = self.primary_finger == Some(finger_id);
                let source = if finger {
                    PointerSource::Touch { finger_id, force }
                } else {
                    PointerSource::Unknown
                };
                events.push(WindowEvent::PointerMoved { device_id, primary, position, source });
            },
            Phase::Up(_) | Phase::Cancel => {
                let primary = self.primary_finger == Some(finger_id);
                if primary {
                    self.primary_finger = None;
                }

                if motion.phase != Phase::Cancel {
                    let button = if finger {
                        ButtonSource::Touch { finger_id, force }
                    } else {
                        ButtonSource::Unknown(0)
                    };
                    events.push(WindowEvent::PointerButton {
                        device_id,
                        primary,
                        state: ElementState::Released,
                        position,
                        button,
                        is_macos_activation_click: false,
                    });
                }

                events.push(WindowEvent::PointerLeft {
                    device_id,
                    primary,
                    position: Some(position),
                    kind,
                });
            },
            _ => {},
        }
    }

    fn hover(&mut self, motion: &Motion, contact: &Contact, events: &mut Vec<WindowEvent>) {
        if !self.devices.contains_key(&motion.device) && self.devices.len() >= MAX_DEVICES {
            return;
        }

        let device_id = Some(DeviceId::from_raw(motion.device));
        let position = contact.position;
        let state = self.devices.entry(motion.device).or_insert(Hovering {
            tool: contact.tool,
            inside: false,
            leaving: false,
            press: Press::Up,
            buttons: 0,
            position,
        });
        state.position = position;
        if state.tool != contact.tool {
            if state.inside {
                events.push(WindowEvent::PointerLeft {
                    device_id,
                    primary: true,
                    position: Some(position),
                    kind: kind_of(state.tool),
                });
            }

            *state = Hovering { tool: contact.tool, inside: false, leaving: false, press: Press::Up, buttons: 0, position };
        }

        if motion.phase == Phase::HoverExit {
            state.leaving = state.inside;
            return;
        }

        state.leaving = false;
        if motion.phase == Phase::Cancel {
            release_all(state, device_id, contact, events);
            if state.inside {
                state.inside = false;
                events.push(WindowEvent::PointerLeft {
                    device_id,
                    primary: true,
                    position: Some(position),
                    kind: kind_of(state.tool),
                });
            }

            return;
        }

        if !state.inside {
            state.inside = true;
            events.push(WindowEvent::PointerEntered {
                device_id,
                primary: true,
                position,
                kind: kind_of(state.tool),
            });
        }

        if matches!(motion.phase, Phase::Move | Phase::HoverMove) {
            events.push(WindowEvent::PointerMoved {
                device_id,
                primary: true,
                position,
                source: source_of(contact),
            });
        }

        let mouse_bits = motion.buttons & MOUSE_BUTTONS.iter().fold(0, |all, (bit, _)| all | bit);
        match motion.phase {
            Phase::Down(_) => {
                state.press = if mouse_bits == 0 { Press::Emulated } else { Press::Down };
            },
            Phase::Up(_) => {
                state.press = Press::Up;
            },
            _ => {},
        }

        if let Some(kind) = tablet_kind(state.tool) {
            stylus_buttons(state, kind, motion, device_id, contact, events);
        } else {
            let mut buttons = motion.buttons;
            if state.press == Press::Emulated {
                buttons |= BUTTON_PRIMARY;
            }

            mouse_buttons(state, buttons, device_id, position, events);
        }
    }
}

/// Reports the changes of the mouse buttons and keeps the new state.
fn mouse_buttons(
    state: &mut Hovering,
    buttons: u32,
    device_id: Option<DeviceId>,
    position: PhysicalPosition<f64>,
    events: &mut Vec<WindowEvent>,
) {
    for (bit, button) in MOUSE_BUTTONS {
        let was = state.buttons & bit != 0;
        let is = buttons & bit != 0;
        if was == is {
            continue;
        }

        events.push(WindowEvent::PointerButton {
            device_id,
            primary: true,
            state: if is { ElementState::Pressed } else { ElementState::Released },
            position,
            button: ButtonSource::Mouse(button),
            is_macos_activation_click: false,
        });
    }

    state.buttons = buttons & MOUSE_BUTTONS.iter().fold(0, |all, (bit, _)| all | bit);
}

/// Reports contact and barrel button changes of a stylus and keeps the new state.
fn stylus_buttons(
    state: &mut Hovering,
    kind: TabletToolKind,
    motion: &Motion,
    device_id: Option<DeviceId>,
    contact: &Contact,
    events: &mut Vec<WindowEvent>,
) {
    let mut push = |pressed: bool, button: TabletToolButton| {
        events.push(WindowEvent::PointerButton {
            device_id,
            primary: true,
            state: if pressed { ElementState::Pressed } else { ElementState::Released },
            position: contact.position,
            button: ButtonSource::TabletTool { kind, button, data: tool_data(contact) },
            is_macos_activation_click: false,
        });
    };

    match motion.phase {
        Phase::Down(_) => push(true, TabletToolButton::Contact),
        Phase::Up(_) => push(false, TabletToolButton::Contact),
        _ => {},
    }

    for (bit, button) in STYLUS_BUTTONS {
        let was = state.buttons & bit != 0;
        let is = motion.buttons & bit != 0;
        if was != is {
            push(is, button);
        }
    }

    state.buttons = motion.buttons & STYLUS_BUTTONS.iter().fold(0, |all, (bit, _)| all | bit);
}

/// Releases every held button of a canceled mouse or stylus.
fn release_all(
    state: &mut Hovering,
    device_id: Option<DeviceId>,
    contact: &Contact,
    events: &mut Vec<WindowEvent>,
) {
    match tablet_kind(state.tool) {
        Some(kind) => {
            let motion = Motion { device: 0, phase: Phase::Other, buttons: 0, contacts: Vec::new() };
            if state.press != Press::Up {
                events.push(WindowEvent::PointerButton {
                    device_id,
                    primary: true,
                    state: ElementState::Released,
                    position: contact.position,
                    button: ButtonSource::TabletTool {
                        kind,
                        button: TabletToolButton::Contact,
                        data: tool_data(contact),
                    },
                    is_macos_activation_click: false,
                });
            }

            stylus_buttons(state, kind, &motion, device_id, contact, events);
        },
        None => mouse_buttons(state, 0, device_id, contact.position, events),
    }

    state.press = Press::Up;
}

/// The pointer kind of a hovering tool.
fn kind_of(tool: Tool) -> PointerKind {
    match tablet_kind(tool) {
        Some(kind) => PointerKind::TabletTool(kind),
        None => PointerKind::Mouse,
    }
}

/// The pointer source of a hovering contact.
fn source_of(contact: &Contact) -> PointerSource {
    match tablet_kind(contact.tool) {
        Some(kind) => PointerSource::TabletTool { kind, data: tool_data(contact) },
        None => PointerSource::Mouse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(tool: Tool, x: f64) -> Contact {
        Contact {
            id: 0,
            tool,
            position: PhysicalPosition::new(x, 1.0),
            pressure: 0.5,
            tilt: 0.0,
            orientation: 0.0,
            hscroll: 0.0,
            vscroll: 0.0,
        }
    }

    fn motion(phase: Phase, buttons: u32, contacts: Vec<Contact>) -> Motion {
        Motion { device: 7, phase, buttons, contacts }
    }

    fn names(events: &[WindowEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                WindowEvent::PointerEntered { kind, .. } => format!("enter {kind:?}"),
                WindowEvent::PointerLeft { kind, .. } => format!("left {kind:?}"),
                WindowEvent::PointerMoved { source, .. } => {
                    format!("move {:?}", PointerKind::from(source.clone()))
                },
                WindowEvent::PointerButton { state, button, .. } => match button {
                    ButtonSource::TabletTool { button, .. } => format!("{state:?} {button:?}"),
                    other => format!("{state:?} {other:?}"),
                },
                WindowEvent::MouseWheel { delta, .. } => format!("wheel {delta:?}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_mouse_click_keeps_one_pointer() {
        let mut pointers = Pointers::default();
        let mouse = |x| vec![contact(Tool::Mouse, x)];
        let mut all = Vec::new();
        all.extend(pointers.handle(&motion(Phase::HoverEnter, 0, mouse(1.0))));
        all.extend(pointers.handle(&motion(Phase::HoverMove, 0, mouse(2.0))));
        all.extend(pointers.handle(&motion(Phase::HoverExit, 0, mouse(2.0))));
        all.extend(pointers.handle(&motion(Phase::Down(0), BUTTON_PRIMARY, mouse(2.0))));
        all.extend(pointers.handle(&motion(Phase::Button, BUTTON_PRIMARY, mouse(2.0))));
        all.extend(pointers.handle(&motion(Phase::Move, BUTTON_PRIMARY, mouse(3.0))));
        all.extend(pointers.handle(&motion(Phase::Button, 0, mouse(3.0))));
        all.extend(pointers.handle(&motion(Phase::Up(0), 0, mouse(3.0))));
        all.extend(pointers.handle(&motion(Phase::HoverEnter, 0, mouse(3.0))));
        all.extend(pointers.finish());
        assert_eq!(
            names(&all),
            vec![
                "enter Mouse",
                "move Mouse",
                "Pressed Mouse(Left)",
                "move Mouse",
                "Released Mouse(Left)",
            ]
        );
    }

    #[test]
    fn a_hover_exit_alone_leaves_at_the_end_of_the_batch() {
        let mut pointers = Pointers::default();
        pointers.handle(&motion(Phase::HoverEnter, 0, vec![contact(Tool::Mouse, 1.0)]));
        assert!(pointers.handle(&motion(Phase::HoverExit, 0, vec![contact(Tool::Mouse, 9.0)])).is_empty());
        let left = pointers.finish();
        assert!(matches!(
            left.as_slice(),
            [WindowEvent::PointerLeft { kind: PointerKind::Mouse, position: Some(p), .. }] if p.x > 8.0
        ));
        assert!(pointers.finish().is_empty());
        assert!(pointers.devices.is_empty());
    }

    #[test]
    fn mouse_buttons_follow_the_button_state() {
        let mut pointers = Pointers::default();
        let mouse = vec![contact(Tool::Mouse, 1.0)];
        pointers.handle(&motion(Phase::HoverEnter, 0, mouse.clone()));
        let events = pointers.handle(&motion(Phase::Button, BUTTON_SECONDARY | BUTTON_BACK, mouse.clone()));
        assert_eq!(names(&events), vec!["Pressed Mouse(Right)", "Pressed Mouse(Back)"]);
        let events = pointers.handle(&motion(Phase::Button, BUTTON_BACK | BUTTON_FORWARD, mouse.clone()));
        assert_eq!(names(&events), vec!["Released Mouse(Right)", "Pressed Mouse(Forward)"]);
        let events = pointers.handle(&motion(Phase::Cancel, BUTTON_BACK | BUTTON_FORWARD, mouse));
        assert_eq!(names(&events), vec!["Released Mouse(Back)", "Released Mouse(Forward)", "left Mouse"]);
    }

    #[test]
    fn a_mouse_press_without_button_state_is_left() {
        let mut pointers = Pointers::default();
        let mouse = vec![contact(Tool::Mouse, 1.0)];
        let events = pointers.handle(&motion(Phase::Down(0), 0, mouse.clone()));
        assert_eq!(names(&events), vec!["enter Mouse", "Pressed Mouse(Left)"]);
        let events = pointers.handle(&motion(Phase::Up(0), 0, mouse));
        assert_eq!(names(&events), vec!["Released Mouse(Left)"]);
    }

    #[test]
    fn a_stylus_hovers_touches_and_uses_its_barrel() {
        let mut pointers = Pointers::default();
        let pen = vec![contact(Tool::Stylus, 1.0)];
        let mut all = Vec::new();
        all.extend(pointers.handle(&motion(Phase::HoverEnter, 0, pen.clone())));
        all.extend(pointers.handle(&motion(Phase::HoverExit, 0, pen.clone())));
        all.extend(pointers.handle(&motion(Phase::Down(0), 0, pen.clone())));
        all.extend(pointers.handle(&motion(Phase::Move, BUTTON_STYLUS_PRIMARY, pen.clone())));
        all.extend(pointers.handle(&motion(Phase::Up(0), 0, pen.clone())));
        all.extend(pointers.finish());
        assert_eq!(
            names(&all),
            vec![
                "enter TabletTool(Pen)",
                "Pressed Contact",
                "move TabletTool(Pen)",
                "Pressed Barrel",
                "Released Contact",
                "Released Barrel",
            ]
        );
    }

    #[test]
    fn an_eraser_replaces_the_pen_of_the_same_device() {
        let mut pointers = Pointers::default();
        pointers.handle(&motion(Phase::HoverEnter, 0, vec![contact(Tool::Stylus, 1.0)]));
        let events = pointers.handle(&motion(Phase::HoverMove, 0, vec![contact(Tool::Eraser, 1.0)]));
        assert_eq!(
            names(&events),
            vec!["left TabletTool(Pen)", "enter TabletTool(Eraser)", "move TabletTool(Eraser)"]
        );
    }

    #[test]
    fn stylus_angles_are_converted() {
        let mut pen = contact(Tool::Stylus, 0.0);
        let upright = angle(&pen).unwrap();
        assert!((upright.altitude - FRAC_PI_2).abs() < 1e-9);
        assert!(upright.azimuth.abs() < 1e-9);

        pen.tilt = 0.5;
        pen.orientation = std::f32::consts::FRAC_PI_2;
        let right = angle(&pen).unwrap();
        assert!((right.altitude - (FRAC_PI_2 - 0.5)).abs() < 1e-6);
        assert!(right.azimuth.abs() < 1e-6);

        pen.orientation = 0.0;
        assert!((angle(&pen).unwrap().azimuth - 3.0 * FRAC_PI_2).abs() < 1e-6);

        pen.tilt = 10.0;
        assert_eq!(angle(&pen).unwrap().altitude, 0.0);
        pen.tilt = f32::NAN;
        assert_eq!(angle(&pen), None);
    }

    #[test]
    fn fingers_report_touches_and_the_primary_one() {
        let mut pointers = Pointers::default();
        let mut second = contact(Tool::Finger, 5.0);
        second.id = 1;
        let first = contact(Tool::Finger, 1.0);
        let events = pointers.handle(&motion(Phase::Down(0), 0, vec![first]));
        assert!(matches!(events.first(), Some(WindowEvent::PointerEntered { primary: true, .. })));
        let events = pointers.handle(&motion(Phase::Down(1), 0, vec![first, second]));
        assert_eq!(events.len(), 2);
        assert!(matches!(events.first(), Some(WindowEvent::PointerEntered { primary: false, .. })));
        let events = pointers.handle(&motion(Phase::Move, 0, vec![first, second]));
        assert_eq!(events.len(), 2);
        let events = pointers.handle(&motion(Phase::Cancel, 0, vec![first, second]));
        assert_eq!(names(&events), vec!["left Touch(FingerId(0))", "left Touch(FingerId(1))"]);
    }

    #[test]
    fn an_unknown_tool_reports_an_unknown_pointer() {
        let mut pointers = Pointers::default();
        let other = vec![contact(Tool::Unknown, 1.0)];
        let events = pointers.handle(&motion(Phase::Down(0), 0, other.clone()));
        assert_eq!(names(&events), vec!["enter Unknown", "Pressed Unknown(0)"]);
        let events = pointers.handle(&motion(Phase::Up(0), 0, other));
        assert_eq!(names(&events), vec!["Released Unknown(0)", "left Unknown"]);
    }

    #[test]
    fn scrolling_is_a_wheel_in_lines() {
        let mut pointers = Pointers::default();
        let mut mouse = contact(Tool::Mouse, 1.0);
        mouse.vscroll = 1.0;
        mouse.hscroll = 2.0;
        let events = pointers.handle(&motion(Phase::Scroll, 0, vec![mouse]));
        assert_eq!(names(&events), vec!["wheel LineDelta(-2.0, 1.0)"]);
    }

    #[test]
    fn hovering_devices_are_limited() {
        let mut pointers = Pointers::default();
        for device in 0..100 {
            let mut hover = motion(Phase::HoverEnter, 0, vec![contact(Tool::Mouse, 1.0)]);
            hover.device = device;
            pointers.handle(&hover);
        }

        assert_eq!(pointers.devices.len(), MAX_DEVICES);
    }
}
