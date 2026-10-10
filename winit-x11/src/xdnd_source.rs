//! Protocol logic of an XDND drag source, independent of the X server connection.

use std::os::raw::c_long;
use std::time::{Duration, Instant};

use winit_core::event_loop::DndAction;

/// The XDND version this source speaks.
pub(crate) const SOURCE_VERSION: u32 = 5;

/// The lowest XDND version of a target this source talks to.
pub(crate) const MIN_TARGET_VERSION: u32 = 3;

/// How long the source waits for `XdndStatus` after `XdndPosition`.
pub(crate) const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the source waits for `XdndFinished` after `XdndDrop`.
pub(crate) const FINISHED_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an `INCR` transfer waits for the requestor to delete the property.
pub(crate) const INCR_TIMEOUT: Duration = Duration::from_secs(10);

/// The most `INCR` transfers served at the same time.
pub(crate) const MAX_INCR_TRANSFERS: usize = 16;

const SHIFT_MASK: u16 = 1;
const CONTROL_MASK: u16 = 1 << 2;

/// `now` plus `duration`, or `now` when the sum does not fit.
pub(crate) fn after(now: Instant, duration: Duration) -> Instant {
    now.checked_add(duration).unwrap_or(now)
}

/// The low 32 bits of a long from an Xlib client message, which carries a `CARD32`.
pub(crate) fn card32(value: c_long) -> u32 {
    let bytes = value.to_le_bytes();
    bytes
        .get(..4)
        .and_then(|low| <[u8; 4]>::try_from(low).ok())
        .map(u32::from_le_bytes)
        .unwrap_or_default()
}

/// Packs a root window point as `(x << 16) | y`.
pub(crate) fn pack_point(x: i16, y: i16) -> u32 {
    let x = u16::from_ne_bytes(x.to_ne_bytes());
    let y = u16::from_ne_bytes(y.to_ne_bytes());
    (u32::from(x) << 16) | u32::from(y)
}

fn high_half(value: u32) -> u16 {
    u16::try_from(value >> 16).unwrap_or_default()
}

fn low_half(value: u32) -> u16 {
    u16::try_from(value & 0xffff).unwrap_or_default()
}

fn signed(value: u16) -> i16 {
    i16::from_ne_bytes(value.to_ne_bytes())
}

/// A rectangle in root window coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Rect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    /// Unpacks the rectangle of `XdndStatus`, `data.l[2]` and `data.l[3]`.
    pub(crate) fn unpack(position: u32, size: u32) -> Self {
        Self {
            x: signed(high_half(position)),
            y: signed(low_half(position)),
            width: high_half(size),
            height: low_half(size),
        }
    }

    /// Whether the point lies inside the rectangle.
    pub(crate) fn contains(self, x: i16, y: i16) -> bool {
        let (x, y) = (i32::from(x), i32::from(y));
        let left = i32::from(self.x);
        let top = i32::from(self.y);
        x >= left
            && y >= top
            && x < left + i32::from(self.width)
            && y < top + i32::from(self.height)
    }
}

/// The version used with a target whose `XdndAware` property is `aware`.
///
/// Returns `None` when the property is empty or the target is older than
/// [`MIN_TARGET_VERSION`].
pub(crate) fn negotiate_version(aware: &[u32]) -> Option<u32> {
    let version = (*aware.first()?).min(SOURCE_VERSION);
    (version >= MIN_TARGET_VERSION).then_some(version)
}

/// The proxy a window's `XdndProxy` property names, when the proxy names itself.
///
/// `proxy` is the `XdndProxy` property of the window, `proxy_of_proxy` the one of the proxy.
pub(crate) fn valid_proxy(proxy: &[u32], proxy_of_proxy: &[u32]) -> Option<u32> {
    let proxy = *proxy.first()?;
    (proxy != 0 && proxy_of_proxy.first() == Some(&proxy)).then_some(proxy)
}

/// The data of `XdndEnter`, and whether `XdndTypeList` is needed.
pub(crate) fn enter_data(source: u32, version: u32, types: &[u32]) -> ([u32; 5], bool) {
    let more = types.len() > 3;
    let flags = (version << 24) | u32::from(more);
    let first = |index: usize| if more { 0 } else { types.get(index).copied().unwrap_or(0) };
    ([source, flags, first(0), first(1), first(2)], more)
}

/// The data of `XdndPosition`.
pub(crate) fn position_data(source: u32, x: i16, y: i16, time: u32, action: u32) -> [u32; 5] {
    [source, 0, pack_point(x, y), time, action]
}

/// The data of `XdndLeave`.
pub(crate) fn leave_data(source: u32) -> [u32; 5] {
    [source, 0, 0, 0, 0]
}

/// The data of `XdndDrop`.
pub(crate) fn drop_data(source: u32, time: u32) -> [u32; 5] {
    [source, 0, time, 0, 0]
}

/// The fields of an `XdndStatus` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatusData {
    pub window: u32,
    pub accept: bool,
    pub want_position: bool,
    pub rect: Rect,
    pub action: u32,
}

impl StatusData {
    pub(crate) fn parse(data: [u32; 5]) -> Self {
        let [window, flags, position, size, action] = data;
        Self {
            window,
            accept: flags & 1 != 0,
            want_position: flags & 2 != 0,
            rect: Rect::unpack(position, size),
            action,
        }
    }
}

/// The fields of an `XdndFinished` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FinishedData {
    pub window: u32,
    pub success: bool,
    pub action: u32,
}

impl FinishedData {
    pub(crate) fn parse(data: [u32; 5]) -> Self {
        let [window, flags, action, _, _] = data;
        Self { window, success: flags & 1 != 0, action }
    }
}

/// The actions of `actions` that XDND supports in this backend, in order and without repeats.
pub(crate) fn supported_actions(actions: &[DndAction]) -> Vec<DndAction> {
    let mut supported = Vec::new();
    for action in actions {
        if matches!(action, DndAction::Copy | DndAction::Move) && !supported.contains(action) {
            supported.push(*action);
        }
    }

    supported
}

/// The action the source requests for the modifier mask of a core event.
///
/// Shift alone asks for move, Control alone for copy, otherwise the first allowed action is
/// requested. A modifier asking for an action that is not allowed is ignored.
pub(crate) fn source_action(allowed: &[DndAction], mask: u16) -> Option<DndAction> {
    let shift = mask & SHIFT_MASK != 0;
    let control = mask & CONTROL_MASK != 0;
    let wanted = match (shift, control) {
        (true, false) => Some(DndAction::Move),
        (false, true) => Some(DndAction::Copy),
        _ => None,
    };
    wanted.filter(|action| allowed.contains(action)).or_else(|| allowed.first().copied())
}

/// The keycodes that produce `keysym` in a core keyboard mapping.
///
/// `keysyms` holds `per_keycode` entries for each keycode, starting at `min_keycode`.
pub(crate) fn keycodes_for(
    keysyms: &[u32],
    per_keycode: u8,
    min_keycode: u8,
    keysym: u32,
) -> Vec<u8> {
    if per_keycode == 0 {
        return Vec::new();
    }

    keysyms
        .chunks(usize::from(per_keycode))
        .enumerate()
        .filter(|(_, row)| row.contains(&keysym))
        .filter_map(|(index, _)| {
            u8::try_from(index).ok().and_then(|offset| min_keycode.checked_add(offset))
        })
        .collect()
}

/// How long after a drag the crossing events of its ungrab are still expected.
pub(crate) const UNGRAB_TIMEOUT: Duration = Duration::from_secs(1);

const NOTIFY_GRAB: i32 = 1;
const NOTIFY_UNGRAB: i32 = 2;

/// The window of a drag grab that was released, and until when its ungrab events are expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EndedGrab {
    pub window: u32,
    pub until: Instant,
}

/// Whether an XI2 crossing or focus event with `mode` on `window` stems from the drag grab.
///
/// `active` is the window of the running drag grab, `ended` the grab released last.
pub(crate) fn from_drag_grab(
    mode: i32,
    window: u32,
    active: Option<u32>,
    ended: Option<EndedGrab>,
    now: Instant,
) -> bool {
    match mode {
        NOTIFY_GRAB | NOTIFY_UNGRAB if active == Some(window) => true,
        NOTIFY_UNGRAB => ended.is_some_and(|ended| ended.window == window && now < ended.until),
        _ => false,
    }
}

/// A target that speaks XDND.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target {
    /// The window under the pointer, named in the messages.
    pub window: u32,
    /// The window the messages are sent to, the proxy or `window` itself.
    pub proxy: u32,
    /// The negotiated version.
    pub version: u32,
}

/// The pointer as seen by the source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Pointer {
    pub x: i16,
    pub y: i16,
    pub time: u32,
    pub action: DndAction,
}

/// An `XdndStatus` with its action mapped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Status {
    pub window: u32,
    pub accept: bool,
    pub want_position: bool,
    pub rect: Rect,
    /// The action the target accepts, `None` when it is not copy or move.
    pub action: Option<DndAction>,
}

/// An `XdndFinished` with its action mapped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Finished {
    pub window: u32,
    pub success: bool,
    pub action: Option<DndAction>,
}

/// A message the source sends to a target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Message {
    Enter,
    Position(Pointer),
    Leave,
    Drop { time: u32 },
}

/// The end of a drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Outcome {
    Dropped(Option<DndAction>),
    Canceled,
}

/// The cursor shown during the drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Feedback {
    NoDrop,
    Copy,
    Move,
}

/// What the machine asks the connection to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Output {
    Send(Target, Message),
    Cursor(Feedback),
    Finish(Outcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Dragging,
    Released { time: u32 },
    Dropped { deadline: Instant },
    Done,
}

#[derive(Debug, Clone, Copy)]
struct Session {
    target: Target,
    waiting: Option<Instant>,
    pending: bool,
    status: Option<Status>,
    sent: Option<Pointer>,
}

impl Session {
    fn new(target: Target) -> Self {
        Self { target, waiting: None, pending: false, status: None, sent: None }
    }

    fn should_send(&self, pointer: Pointer) -> bool {
        let Some(sent) = self.sent else {
            return true;
        };
        if sent.action != pointer.action {
            return true;
        }

        let quiet = self.status.is_some_and(|status| {
            !status.want_position && status.rect.contains(pointer.x, pointer.y)
        });
        !quiet && (sent.x != pointer.x || sent.y != pointer.y)
    }
}

/// The state machine of one outgoing drag.
#[derive(Debug)]
pub(crate) struct Machine {
    allowed: Vec<DndAction>,
    phase: Phase,
    session: Option<Session>,
    blocked: Option<u32>,
    pointer: Option<Pointer>,
    feedback: Feedback,
    deleted: bool,
}

impl Machine {
    /// A drag allowing `allowed`, which holds only copy and move.
    pub(crate) fn new(allowed: Vec<DndAction>) -> Self {
        Self {
            allowed,
            phase: Phase::Dragging,
            session: None,
            blocked: None,
            pointer: None,
            feedback: Feedback::NoDrop,
            deleted: false,
        }
    }

    /// The target converted `DELETE`. Returns whether the request is answered, which it is
    /// only between `XdndDrop` and `XdndFinished`.
    pub(crate) fn delete(&mut self) -> bool {
        let dropped = matches!(self.phase, Phase::Dropped { .. });
        if dropped {
            self.deleted = true;
        }
        dropped
    }

    /// The actions the drag allows.
    pub(crate) fn allowed(&self) -> &[DndAction] {
        &self.allowed
    }

    /// Whether the drag has ended.
    pub(crate) fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    /// Whether the pointer is still dragging, before release.
    pub(crate) fn is_dragging(&self) -> bool {
        self.phase == Phase::Dragging
    }

    /// The current target.
    pub(crate) fn target(&self) -> Option<Target> {
        self.session.map(|session| session.target)
    }

    /// The next instant at which [`Machine::tick`] has work to do.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Dropped { deadline } => Some(deadline),
            Phase::Done => None,
            Phase::Dragging | Phase::Released { .. } => {
                self.session.and_then(|session| session.waiting)
            },
        }
    }

    fn accepted_action(&self) -> Option<DndAction> {
        let status = self.session?.status?;
        status.action.filter(|action| status.accept && self.allowed.contains(action))
    }

    fn update_feedback(&mut self, out: &mut Vec<Output>) {
        let feedback = match self.accepted_action() {
            Some(DndAction::Move) => Feedback::Move,
            Some(_) => Feedback::Copy,
            None => Feedback::NoDrop,
        };
        if feedback != self.feedback {
            self.feedback = feedback;
            out.push(Output::Cursor(feedback));
        }
    }

    fn finish(&mut self, outcome: Outcome, out: &mut Vec<Output>) {
        self.phase = Phase::Done;
        out.push(Output::Finish(outcome));
    }

    fn send_position(&mut self, now: Instant, out: &mut Vec<Output>) {
        let (Some(session), Some(pointer)) = (self.session.as_mut(), self.pointer) else {
            return;
        };
        if session.waiting.is_some() {
            session.pending = true;
            return;
        }

        session.pending = false;
        if session.should_send(pointer) {
            session.sent = Some(pointer);
            session.waiting = Some(after(now, STATUS_TIMEOUT));
            out.push(Output::Send(session.target, Message::Position(pointer)));
        }
    }

    fn leave(&mut self, out: &mut Vec<Output>) {
        if let Some(session) = self.session.take() {
            out.push(Output::Send(session.target, Message::Leave));
        }

        self.update_feedback(out);
    }

    /// The pointer moved to `pointer` over `target`, or the requested action changed.
    pub(crate) fn motion(
        &mut self,
        now: Instant,
        target: Option<Target>,
        pointer: Pointer,
    ) -> Vec<Output> {
        let mut out = Vec::new();
        if self.phase != Phase::Dragging {
            return out;
        }

        self.pointer = Some(pointer);
        let target = match target {
            Some(target) if self.blocked == Some(target.window) => None,
            other => {
                self.blocked = None;
                other
            },
        };
        if self.target() != target {
            self.leave(&mut out);
            if let Some(target) = target {
                self.session = Some(Session::new(target));
                out.push(Output::Send(target, Message::Enter));
            }
        }

        self.send_position(now, &mut out);
        out
    }

    /// The target answered with `XdndStatus`.
    pub(crate) fn status(&mut self, now: Instant, status: Status) -> Vec<Output> {
        let mut out = Vec::new();
        let phase = self.phase;
        if matches!(phase, Phase::Dropped { .. } | Phase::Done) {
            return out;
        }

        let Some(session) = self.session.as_mut() else {
            return out;
        };
        if session.target.window != status.window {
            return out;
        }

        session.waiting = None;
        session.status = Some(status);
        let pending = session.pending;
        self.update_feedback(&mut out);
        match phase {
            Phase::Released { time } => self.drop_or_cancel(now, time, &mut out),
            _ if pending => self.send_position(now, &mut out),
            _ => {},
        }

        out
    }

    fn drop_or_cancel(&mut self, now: Instant, time: u32, out: &mut Vec<Output>) {
        let accepted = self.accepted_action().is_some();
        match self.session {
            Some(session) if accepted => {
                out.push(Output::Send(session.target, Message::Drop { time }));
                self.phase = Phase::Dropped { deadline: after(now, FINISHED_TIMEOUT) };
            },
            _ => {
                self.leave(out);
                self.finish(Outcome::Canceled, out);
            },
        }
    }

    /// The pointer button was released at server time `time`.
    pub(crate) fn release(&mut self, now: Instant, time: u32) -> Vec<Output> {
        let mut out = Vec::new();
        if self.phase != Phase::Dragging {
            return out;
        }

        self.phase = Phase::Released { time };
        let waiting_with_status = self
            .session
            .is_some_and(|session| session.waiting.is_some() && session.status.is_some());
        if !waiting_with_status {
            self.drop_or_cancel(now, time, &mut out);
        }

        out
    }

    /// The target answered the drop with `XdndFinished`.
    pub(crate) fn finished(&mut self, finished: Finished) -> Vec<Output> {
        let mut out = Vec::new();
        if !matches!(self.phase, Phase::Dropped { .. }) {
            return out;
        }

        let Some(session) = self.session else {
            return out;
        };
        if session.target.window != finished.window {
            return out;
        }

        let outcome = if session.target.version >= 5 {
            if finished.success { Outcome::Dropped(finished.action) } else { Outcome::Canceled }
        } else {
            Outcome::Dropped(self.accepted_action())
        };
        let outcome = match outcome {
            Outcome::Dropped(Some(DndAction::Move)) if !self.deleted => {
                Outcome::Dropped(Some(DndAction::Copy))
            },
            other => other,
        };
        self.finish(outcome, &mut out);
        out
    }

    /// Aborts the drag, on Escape or when the selection is lost.
    pub(crate) fn cancel(&mut self) -> Vec<Output> {
        let mut out = Vec::new();
        match self.phase {
            Phase::Done => {},
            Phase::Dropped { .. } => self.finish(Outcome::Canceled, &mut out),
            Phase::Dragging | Phase::Released { .. } => {
                self.leave(&mut out);
                self.finish(Outcome::Canceled, &mut out);
            },
        }

        out
    }

    /// Handles the timeouts that expired at `now`.
    pub(crate) fn tick(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        match self.phase {
            Phase::Done => {},
            Phase::Dropped { deadline } => {
                if now >= deadline {
                    self.finish(Outcome::Canceled, &mut out);
                }
            },
            Phase::Dragging | Phase::Released { .. } => {
                let expired = self
                    .session
                    .and_then(|session| session.waiting)
                    .is_some_and(|deadline| now >= deadline);
                if expired {
                    self.blocked = self.target().map(|target| target.window);
                    self.leave(&mut out);
                    if !self.is_dragging() {
                        self.finish(Outcome::Canceled, &mut out);
                    }
                }
            },
        }

        out
    }
}

/// An `INCR` transfer of one converted selection.
#[derive(Debug)]
pub(crate) struct Incr {
    pub requestor: u32,
    pub property: u32,
    pub type_: u32,
    pub deadline: Instant,
    data: Vec<u8>,
    chunk: usize,
    offset: usize,
    ended: bool,
}

impl Incr {
    /// A transfer of `data` in pieces of at most `chunk` bytes.
    pub(crate) fn new(
        requestor: u32,
        property: u32,
        type_: u32,
        data: Vec<u8>,
        chunk: usize,
        now: Instant,
    ) -> Self {
        Self {
            requestor,
            property,
            type_,
            deadline: after(now, INCR_TIMEOUT),
            data,
            chunk: chunk.max(1),
            offset: 0,
            ended: false,
        }
    }

    /// The total length announced in the `INCR` property.
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }

    /// The next piece after the requestor deleted the property, empty for the final one.
    ///
    /// Returns `None` once the final empty piece was handed out.
    pub(crate) fn next_piece(&mut self, now: Instant) -> Option<&[u8]> {
        if self.ended {
            return None;
        }

        let start = self.offset;
        let end = start.saturating_add(self.chunk).min(self.data.len());
        if start >= end {
            self.ended = true;
        }

        self.offset = end;
        self.deadline = after(now, INCR_TIMEOUT);
        self.data.get(start..end.max(start))
    }
}

/// Encodes URIs as `text/uri-list`, each line ending in CRLF.
pub(crate) fn encode_uri_list(uris: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for uri in uris {
        out.extend_from_slice(uri.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    out
}

/// Encodes text as ISO-8859-1, `None` when a character lies outside of it.
pub(crate) fn encode_latin1(text: &str) -> Option<Vec<u8>> {
    text.chars().map(|c| u8::try_from(u32::from(c)).ok()).collect()
}

/// Whether a selection request made at `request` is answered by an owner since `owned`.
///
/// A request at `CurrentTime` (zero) is always answered.
pub(crate) fn request_in_time(owned: u32, request: u32) -> bool {
    request == 0 || i32::from_ne_bytes(request.wrapping_sub(owned).to_ne_bytes()) >= 0
}

/// Converts straight RGBA pixels to premultiplied ARGB words, byte swapped when `swap` is set.
pub(crate) fn premultiplied_argb(rgba: &[u8], swap: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        let &[r, g, b, a] = pixel else {
            continue;
        };
        let scale = |c: u8| u8::try_from(u16::from(c) * u16::from(a) / 255).unwrap_or(u8::MAX);
        let word = u32::from_be_bytes([a, scale(r), scale(g), scale(b)]);
        let word = if swap { word.swap_bytes() } else { word };
        out.extend_from_slice(&word.to_ne_bytes());
    }

    out
}

/// A horizontal run of visible pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Run {
    pub x: u16,
    pub y: u16,
    pub width: u16,
}

/// The runs of pixels with nonzero alpha in an RGBA image `width` pixels wide.
pub(crate) fn opaque_runs(rgba: &[u8], width: u16) -> Vec<Run> {
    let mut runs = Vec::new();
    let row_bytes = usize::from(width).saturating_mul(4);
    if row_bytes == 0 {
        return runs;
    }

    for (y, row) in rgba.chunks_exact(row_bytes).enumerate() {
        let Ok(y) = u16::try_from(y) else {
            break;
        };
        let mut start: Option<u16> = None;
        for (x, pixel) in (0..width).zip(row.chunks_exact(4)) {
            let visible = pixel.get(3).is_some_and(|alpha| *alpha != 0);
            match (visible, start) {
                (true, None) => start = Some(x),
                (false, Some(begin)) => {
                    runs.push(Run { x: begin, y, width: x.saturating_sub(begin) });
                    start = None;
                },
                _ => {},
            }
        }

        if let Some(begin) = start {
            runs.push(Run { x: begin, y, width: width.saturating_sub(begin) });
        }
    }

    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: u32 = 0x40_0001;
    const TARGET: Target = Target { window: 0x60_0002, proxy: 0x60_0002, version: 5 };
    const OTHER: Target = Target { window: 0x70_0003, proxy: 0x70_0009, version: 4 };

    fn pointer(x: i16, y: i16, action: DndAction) -> Pointer {
        Pointer { x, y, time: 100, action }
    }

    fn status(target: Target, accept: bool, action: Option<DndAction>) -> Status {
        Status { window: target.window, accept, want_position: true, rect: Rect::default(), action }
    }

    fn quiet(target: Target) -> Status {
        Status {
            want_position: false,
            rect: Rect { x: 0, y: 0, width: 10, height: 10 },
            ..status(target, true, Some(DndAction::Copy))
        }
    }

    fn copy_move() -> Machine {
        Machine::new(vec![DndAction::Copy, DndAction::Move])
    }

    fn dropped(machine: &mut Machine, target: Target, action: DndAction, now: Instant) {
        machine.motion(now, Some(target), pointer(1, 1, action));
        machine.status(now, status(target, true, Some(action)));
        machine.release(now, 5);
    }

    #[test]
    fn card32_takes_the_low_bits() {
        assert_eq!(card32(5), 5);
        assert_eq!(card32(0), 0);
        assert_eq!(card32(-1), u32::MAX);
        assert_eq!(card32(c_long::from(i32::MIN)), 0x8000_0000);
        assert_eq!(card32(c_long::from(i32::MAX)), 0x7fff_ffff);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn card32_drops_the_high_bits() {
        assert_eq!(card32(0x1_0000_0002), 2);
        assert_eq!(card32(c_long::MIN), 0);
    }

    #[test]
    fn points_pack_and_rects_unpack() {
        assert_eq!(pack_point(1, 2), 0x0001_0002);
        assert_eq!(pack_point(-1, 0), 0xffff_0000);
        let rect = Rect::unpack(pack_point(-5, 10), (20 << 16) | 30);
        assert_eq!(rect, Rect { x: -5, y: 10, width: 20, height: 30 });
        assert!(rect.contains(-5, 10));
        assert!(rect.contains(14, 39));
        assert!(!rect.contains(15, 10));
        assert!(!rect.contains(0, 40));
        assert!(!Rect::default().contains(0, 0));
        let edge = Rect { x: i16::MAX, y: i16::MAX, width: u16::MAX, height: u16::MAX };
        assert!(edge.contains(i16::MAX, i16::MAX));
    }

    #[test]
    fn versions_are_negotiated() {
        assert_eq!(negotiate_version(&[5]), Some(5));
        assert_eq!(negotiate_version(&[4]), Some(4));
        assert_eq!(negotiate_version(&[3]), Some(3));
        assert_eq!(negotiate_version(&[7, 1]), Some(5));
        assert_eq!(negotiate_version(&[u32::MAX]), Some(5));
        assert_eq!(negotiate_version(&[2]), None);
        assert_eq!(negotiate_version(&[0]), None);
        assert_eq!(negotiate_version(&[]), None);
    }

    #[test]
    fn proxies_must_name_themselves() {
        assert_eq!(valid_proxy(&[9], &[9]), Some(9));
        assert_eq!(valid_proxy(&[9], &[8]), None);
        assert_eq!(valid_proxy(&[9], &[]), None);
        assert_eq!(valid_proxy(&[], &[]), None);
        assert_eq!(valid_proxy(&[0], &[0]), None);
    }

    #[test]
    fn enter_lists_up_to_three_types() {
        assert_eq!(enter_data(SOURCE, 5, &[]), ([SOURCE, 5 << 24, 0, 0, 0], false));
        assert_eq!(enter_data(SOURCE, 4, &[7, 8]), ([SOURCE, 4 << 24, 7, 8, 0], false));
        assert_eq!(enter_data(SOURCE, 5, &[7, 8, 9]), ([SOURCE, 5 << 24, 7, 8, 9], false));
        assert_eq!(enter_data(SOURCE, 5, &[7, 8, 9, 10]), ([SOURCE, (5 << 24) | 1, 0, 0, 0], true));
    }

    #[test]
    fn messages_pack() {
        assert_eq!(position_data(SOURCE, 3, 4, 77, 12), [SOURCE, 0, 0x0003_0004, 77, 12]);
        assert_eq!(leave_data(SOURCE), [SOURCE, 0, 0, 0, 0]);
        assert_eq!(drop_data(SOURCE, 88), [SOURCE, 0, 88, 0, 0]);
    }

    #[test]
    fn status_and_finished_parse() {
        let parsed = StatusData::parse([9, 3, pack_point(1, 2), (3 << 16) | 4, 42]);
        assert_eq!(parsed, StatusData {
            window: 9,
            accept: true,
            want_position: true,
            rect: Rect { x: 1, y: 2, width: 3, height: 4 },
            action: 42,
        });
        let rejected = StatusData::parse([9, 2, 0, 0, 0]);
        assert!(!rejected.accept);
        assert!(rejected.want_position);
        let reserved = StatusData::parse([9, 0xffff_fffc, 0, 0, 0]);
        assert!(!reserved.accept && !reserved.want_position);
        assert_eq!(FinishedData::parse([9, 1, 42, 5, 6]), FinishedData {
            window: 9,
            success: true,
            action: 42,
        });
        assert!(!FinishedData::parse([9, 0, 0, 0, 0]).success);
    }

    #[test]
    fn only_copy_and_move_are_supported() {
        assert_eq!(
            supported_actions(&[DndAction::Link, DndAction::Move, DndAction::Copy, DndAction::Move]),
            vec![DndAction::Move, DndAction::Copy]
        );
        assert!(supported_actions(&[DndAction::Ask, DndAction::Private]).is_empty());
        assert!(supported_actions(&[]).is_empty());
    }

    #[test]
    fn modifiers_pick_the_action() {
        let both = [DndAction::Move, DndAction::Copy];
        assert_eq!(source_action(&both, 0), Some(DndAction::Move));
        assert_eq!(source_action(&both, CONTROL_MASK), Some(DndAction::Copy));
        assert_eq!(source_action(&both, SHIFT_MASK), Some(DndAction::Move));
        assert_eq!(source_action(&both, SHIFT_MASK | CONTROL_MASK), Some(DndAction::Move));
        assert_eq!(source_action(&[DndAction::Copy], SHIFT_MASK), Some(DndAction::Copy));
        assert_eq!(source_action(&[DndAction::Move], CONTROL_MASK), Some(DndAction::Move));
        let copy_first = [DndAction::Copy, DndAction::Move];
        assert_eq!(source_action(&copy_first, 1 << 3), Some(DndAction::Copy));
        assert_eq!(source_action(&[], SHIFT_MASK), None);
    }

    #[test]
    fn keycodes_are_found() {
        let keysyms = [1, 2, 0xff1b, 0, 3, 0xff1b];
        assert_eq!(keycodes_for(&keysyms, 2, 8, 0xff1b), vec![9, 10]);
        assert_eq!(keycodes_for(&keysyms, 3, 8, 0xff1b), vec![8, 9]);
        assert!(keycodes_for(&keysyms, 0, 8, 0xff1b).is_empty());
        assert!(keycodes_for(&[], 2, 8, 0xff1b).is_empty());
        assert!(keycodes_for(&keysyms, 1, 254, 0xff1b).is_empty());
    }

    #[test]
    fn grab_crossings_are_recognised() {
        let now = Instant::now();
        let ended = Some(EndedGrab { window: 7, until: now + UNGRAB_TIMEOUT });
        assert!(from_drag_grab(NOTIFY_GRAB, 7, Some(7), None, now));
        assert!(from_drag_grab(NOTIFY_UNGRAB, 7, Some(7), None, now));
        assert!(!from_drag_grab(0, 7, Some(7), None, now));
        assert!(!from_drag_grab(3, 7, Some(7), None, now));
        assert!(!from_drag_grab(NOTIFY_GRAB, 8, Some(7), None, now));
        assert!(from_drag_grab(NOTIFY_UNGRAB, 7, None, ended, now));
        assert!(!from_drag_grab(NOTIFY_GRAB, 7, None, ended, now));
        assert!(!from_drag_grab(NOTIFY_UNGRAB, 8, None, ended, now));
        assert!(!from_drag_grab(NOTIFY_UNGRAB, 7, None, ended, now + UNGRAB_TIMEOUT));
        assert!(!from_drag_grab(NOTIFY_UNGRAB, 7, None, None, now));
    }

    #[test]
    fn a_full_drop_reports_the_action() {
        let now = Instant::now();
        let mut machine = copy_move();
        let p = pointer(10, 10, DndAction::Copy);
        assert_eq!(machine.motion(now, Some(TARGET), p), vec![
            Output::Send(TARGET, Message::Enter),
            Output::Send(TARGET, Message::Position(p)),
        ]);
        assert_eq!(machine.deadline(), Some(now + STATUS_TIMEOUT));
        assert_eq!(machine.status(now, status(TARGET, true, Some(DndAction::Copy))), vec![
            Output::Cursor(Feedback::Copy)
        ]);
        assert_eq!(machine.deadline(), None);
        assert_eq!(machine.release(now, 200), vec![Output::Send(TARGET, Message::Drop {
            time: 200
        })]);
        assert_eq!(machine.deadline(), Some(now + FINISHED_TIMEOUT));
        let finished =
            Finished { window: TARGET.window, success: true, action: Some(DndAction::Copy) };
        assert_eq!(machine.finished(finished), vec![Output::Finish(Outcome::Dropped(Some(
            DndAction::Copy
        )))]);
        assert!(machine.is_done());
        assert!(machine.finished(finished).is_empty());
        assert!(machine.cancel().is_empty());
        assert_eq!(machine.deadline(), None);
    }

    #[test]
    fn finished_reports_the_performed_action() {
        let now = Instant::now();
        let mut machine = copy_move();
        dropped(&mut machine, TARGET, DndAction::Move, now);
        let finished =
            Finished { window: TARGET.window, success: true, action: Some(DndAction::Copy) };
        assert_eq!(machine.finished(finished), vec![Output::Finish(Outcome::Dropped(Some(
            DndAction::Copy
        )))]);
    }

    #[test]
    fn a_failed_finish_is_a_cancel() {
        let now = Instant::now();
        let mut machine = copy_move();
        dropped(&mut machine, TARGET, DndAction::Copy, now);
        let finished = Finished { window: TARGET.window, success: false, action: None };
        assert_eq!(machine.finished(finished), vec![Output::Finish(Outcome::Canceled)]);
    }

    #[test]
    fn version_four_finish_reports_the_accepted_action() {
        let now = Instant::now();
        let mut machine = copy_move();
        dropped(&mut machine, OTHER, DndAction::Move, now);
        assert!(machine.delete());
        let finished = Finished { window: OTHER.window, success: false, action: None };
        assert_eq!(machine.finished(finished), vec![Output::Finish(Outcome::Dropped(Some(
            DndAction::Move
        )))]);
    }

    #[test]
    fn a_move_without_delete_is_a_copy() {
        let now = Instant::now();
        let mut machine = copy_move();
        dropped(&mut machine, TARGET, DndAction::Move, now);
        let finished =
            Finished { window: TARGET.window, success: true, action: Some(DndAction::Move) };
        assert_eq!(machine.finished(finished), vec![Output::Finish(Outcome::Dropped(Some(
            DndAction::Copy
        )))]);

        let mut machine = copy_move();
        dropped(&mut machine, OTHER, DndAction::Move, now);
        let finished = Finished { window: OTHER.window, success: true, action: None };
        assert_eq!(machine.finished(finished), vec![Output::Finish(Outcome::Dropped(Some(
            DndAction::Copy
        )))]);
    }

    #[test]
    fn a_move_with_delete_is_a_move() {
        let now = Instant::now();
        let mut machine = copy_move();
        dropped(&mut machine, TARGET, DndAction::Move, now);
        assert!(machine.delete());
        let finished =
            Finished { window: TARGET.window, success: true, action: Some(DndAction::Move) };
        assert_eq!(machine.finished(finished), vec![Output::Finish(Outcome::Dropped(Some(
            DndAction::Move
        )))]);
    }

    #[test]
    fn delete_is_refused_outside_the_drop() {
        let now = Instant::now();
        let mut machine = copy_move();
        assert!(!machine.delete());
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Move));
        assert!(!machine.delete());
        dropped(&mut machine, TARGET, DndAction::Move, now);
        machine.finished(Finished { window: TARGET.window, success: true, action: None });
        assert!(!machine.delete());
    }

    #[test]
    fn delete_does_not_turn_a_copy_into_a_move() {
        let now = Instant::now();
        let mut machine = copy_move();
        dropped(&mut machine, TARGET, DndAction::Copy, now);
        assert!(machine.delete());
        let finished =
            Finished { window: TARGET.window, success: true, action: Some(DndAction::Copy) };
        assert_eq!(machine.finished(finished), vec![Output::Finish(Outcome::Dropped(Some(
            DndAction::Copy
        )))]);
    }

    #[test]
    fn messages_from_other_windows_are_ignored() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        assert!(machine.status(now, status(OTHER, true, Some(DndAction::Copy))).is_empty());
        assert_eq!(machine.deadline(), Some(now + STATUS_TIMEOUT));
        machine.status(now, status(TARGET, true, Some(DndAction::Copy)));
        machine.release(now, 5);
        let wrong = Finished { window: OTHER.window, success: true, action: None };
        assert!(machine.finished(wrong).is_empty());
        assert!(!machine.is_done());
    }

    #[test]
    fn positions_wait_for_status() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        assert!(machine.motion(now, Some(TARGET), pointer(2, 2, DndAction::Copy)).is_empty());
        assert!(machine.motion(now, Some(TARGET), pointer(3, 3, DndAction::Copy)).is_empty());
        let latest = pointer(3, 3, DndAction::Copy);
        assert_eq!(machine.status(now, status(TARGET, false, None)), vec![Output::Send(
            TARGET,
            Message::Position(latest)
        )]);
    }

    #[test]
    fn the_rectangle_suppresses_positions() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(5, 5, DndAction::Copy));
        machine.status(now, quiet(TARGET));
        assert!(machine.motion(now, Some(TARGET), pointer(6, 6, DndAction::Copy)).is_empty());
        let outside = pointer(11, 6, DndAction::Copy);
        assert_eq!(machine.motion(now, Some(TARGET), outside), vec![Output::Send(
            TARGET,
            Message::Position(outside)
        )]);
    }

    #[test]
    fn an_action_change_is_sent_inside_the_rectangle() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(5, 5, DndAction::Copy));
        machine.status(now, quiet(TARGET));
        let moved = pointer(5, 5, DndAction::Move);
        assert_eq!(machine.motion(now, Some(TARGET), moved), vec![Output::Send(
            TARGET,
            Message::Position(moved)
        )]);
    }

    #[test]
    fn an_unchanged_pointer_sends_nothing() {
        let now = Instant::now();
        let mut machine = copy_move();
        let p = pointer(5, 5, DndAction::Copy);
        machine.motion(now, Some(TARGET), p);
        machine.status(now, status(TARGET, true, Some(DndAction::Copy)));
        assert!(machine.motion(now, Some(TARGET), p).is_empty());
    }

    #[test]
    fn changing_targets_leaves_and_enters() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        machine.status(now, status(TARGET, true, Some(DndAction::Copy)));
        let p = pointer(50, 50, DndAction::Copy);
        assert_eq!(machine.motion(now, Some(OTHER), p), vec![
            Output::Send(TARGET, Message::Leave),
            Output::Cursor(Feedback::NoDrop),
            Output::Send(OTHER, Message::Enter),
            Output::Send(OTHER, Message::Position(p)),
        ]);
        let away = pointer(90, 90, DndAction::Copy);
        assert_eq!(machine.motion(now, None, away), vec![Output::Send(OTHER, Message::Leave)]);
        assert!(machine.motion(now, None, pointer(91, 91, DndAction::Copy)).is_empty());
        assert_eq!(machine.target(), None);
    }

    #[test]
    fn release_over_nothing_cancels() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, None, pointer(1, 1, DndAction::Copy));
        assert_eq!(machine.release(now, 5), vec![Output::Finish(Outcome::Canceled)]);
        assert!(machine.is_done());
    }

    #[test]
    fn release_over_a_rejecting_target_leaves() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        machine.status(now, status(TARGET, false, None));
        assert_eq!(machine.release(now, 5), vec![
            Output::Send(TARGET, Message::Leave),
            Output::Finish(Outcome::Canceled),
        ]);
    }

    #[test]
    fn release_without_any_status_leaves_at_once() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        assert_eq!(machine.release(now, 5), vec![
            Output::Send(TARGET, Message::Leave),
            Output::Finish(Outcome::Canceled),
        ]);
    }

    #[test]
    fn release_waits_for_the_last_status() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        machine.status(now, status(TARGET, true, Some(DndAction::Copy)));
        machine.motion(now, Some(TARGET), pointer(2, 2, DndAction::Copy));
        assert!(machine.release(now, 9).is_empty());
        assert!(machine.motion(now, Some(TARGET), pointer(3, 3, DndAction::Copy)).is_empty());
        assert!(machine.release(now, 10).is_empty());
        assert_eq!(machine.status(now, status(TARGET, true, Some(DndAction::Copy))), vec![
            Output::Send(TARGET, Message::Drop { time: 9 })
        ]);
    }

    #[test]
    fn release_waiting_on_a_rejection_cancels() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        machine.status(now, status(TARGET, true, Some(DndAction::Copy)));
        machine.motion(now, Some(TARGET), pointer(2, 2, DndAction::Copy));
        machine.release(now, 9);
        assert_eq!(machine.status(now, status(TARGET, false, None)), vec![
            Output::Cursor(Feedback::NoDrop),
            Output::Send(TARGET, Message::Leave),
            Output::Finish(Outcome::Canceled),
        ]);
    }

    #[test]
    fn an_action_outside_the_allowed_set_is_a_rejection() {
        let now = Instant::now();
        let mut machine = Machine::new(vec![DndAction::Move]);
        assert_eq!(machine.allowed(), &[DndAction::Move]);
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Move));
        assert!(machine.status(now, status(TARGET, true, Some(DndAction::Copy))).is_empty());
        assert!(machine.status(now, status(TARGET, true, None)).is_empty());
        assert_eq!(machine.release(now, 5), vec![
            Output::Send(TARGET, Message::Leave),
            Output::Finish(Outcome::Canceled),
        ]);
    }

    #[test]
    fn move_shows_the_move_cursor() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Move));
        assert_eq!(machine.status(now, status(TARGET, true, Some(DndAction::Move))), vec![
            Output::Cursor(Feedback::Move)
        ]);
    }

    #[test]
    fn a_silent_target_is_left_and_blocked() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        assert!(machine.tick(now + STATUS_TIMEOUT / 2).is_empty());
        assert_eq!(machine.tick(now + STATUS_TIMEOUT), vec![Output::Send(TARGET, Message::Leave)]);
        assert!(!machine.is_done());
        assert_eq!(machine.deadline(), None);
        assert!(machine.motion(now, Some(TARGET), pointer(2, 2, DndAction::Copy)).is_empty());
        machine.motion(now, None, pointer(3, 3, DndAction::Copy));
        let back = machine.motion(now, Some(TARGET), pointer(4, 4, DndAction::Copy));
        assert_eq!(back.first(), Some(&Output::Send(TARGET, Message::Enter)));
    }

    #[test]
    fn a_silent_target_after_release_cancels() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        machine.status(now, status(TARGET, true, Some(DndAction::Copy)));
        machine.motion(now, Some(TARGET), pointer(2, 2, DndAction::Copy));
        machine.release(now, 5);
        assert_eq!(machine.tick(now + STATUS_TIMEOUT), vec![
            Output::Send(TARGET, Message::Leave),
            Output::Cursor(Feedback::NoDrop),
            Output::Finish(Outcome::Canceled),
        ]);
    }

    #[test]
    fn a_missing_finish_times_out() {
        let now = Instant::now();
        let mut machine = copy_move();
        dropped(&mut machine, TARGET, DndAction::Copy, now);
        assert!(machine.tick(now + FINISHED_TIMEOUT / 2).is_empty());
        assert_eq!(machine.tick(now + FINISHED_TIMEOUT), vec![Output::Finish(Outcome::Canceled)]);
        assert!(machine.tick(now + FINISHED_TIMEOUT * 2).is_empty());
    }

    #[test]
    fn cancel_leaves_the_target() {
        let now = Instant::now();
        let mut machine = copy_move();
        machine.motion(now, Some(TARGET), pointer(1, 1, DndAction::Copy));
        machine.status(now, status(TARGET, true, Some(DndAction::Copy)));
        assert_eq!(machine.cancel(), vec![
            Output::Send(TARGET, Message::Leave),
            Output::Cursor(Feedback::NoDrop),
            Output::Finish(Outcome::Canceled),
        ]);
        assert!(machine.motion(now, Some(TARGET), pointer(2, 2, DndAction::Copy)).is_empty());
        assert!(machine.release(now, 5).is_empty());
    }

    #[test]
    fn cancel_after_drop_sends_no_leave() {
        let now = Instant::now();
        let mut machine = copy_move();
        dropped(&mut machine, TARGET, DndAction::Copy, now);
        assert_eq!(machine.cancel(), vec![Output::Finish(Outcome::Canceled)]);
    }

    #[test]
    fn status_after_drop_is_ignored() {
        let now = Instant::now();
        let mut machine = copy_move();
        dropped(&mut machine, TARGET, DndAction::Copy, now);
        assert!(machine.status(now, status(TARGET, false, None)).is_empty());
        assert!(!machine.is_dragging());
    }

    #[test]
    fn incr_hands_out_pieces_then_an_empty_one() {
        let now = Instant::now();
        let mut incr = Incr::new(1, 2, 3, vec![1, 2, 3, 4, 5], 2, now);
        assert_eq!(incr.len(), 5);
        assert_eq!(incr.next_piece(now), Some(&[1, 2][..]));
        assert_eq!(incr.next_piece(now), Some(&[3, 4][..]));
        assert_eq!(incr.next_piece(now), Some(&[5][..]));
        assert_eq!(incr.next_piece(now), Some(&[][..]));
        assert_eq!(incr.next_piece(now), None);
    }

    #[test]
    fn incr_handles_empty_data_and_zero_chunks() {
        let now = Instant::now();
        let mut empty = Incr::new(1, 2, 3, Vec::new(), 4, now);
        assert_eq!(empty.next_piece(now), Some(&[][..]));
        assert_eq!(empty.next_piece(now), None);
        let mut zero = Incr::new(1, 2, 3, vec![7, 8], 0, now);
        assert_eq!(zero.next_piece(now), Some(&[7][..]));
        assert_eq!(zero.next_piece(now), Some(&[8][..]));
        assert_eq!(zero.next_piece(now), Some(&[][..]));
        assert_eq!(zero.next_piece(now), None);
    }

    #[test]
    fn incr_progress_extends_the_deadline() {
        let now = Instant::now();
        let mut incr = Incr::new(1, 2, 3, vec![1, 2], 1, now);
        assert_eq!(incr.deadline, now + INCR_TIMEOUT);
        let later = now + Duration::from_secs(1);
        incr.next_piece(later);
        assert_eq!(incr.deadline, later + INCR_TIMEOUT);
    }

    #[test]
    fn uri_lists_end_lines_in_crlf() {
        let uris = vec!["file:///a%20b".to_owned(), "file:///c".to_owned()];
        assert_eq!(encode_uri_list(&uris), b"file:///a%20b\r\nfile:///c\r\n".to_vec());
        assert!(encode_uri_list(&[]).is_empty());
    }

    #[test]
    fn latin1_rejects_wider_characters() {
        assert_eq!(encode_latin1("abc"), Some(b"abc".to_vec()));
        assert_eq!(encode_latin1("\u{e4}\u{ff}"), Some(vec![0xe4, 0xff]));
        assert_eq!(encode_latin1(""), Some(Vec::new()));
        assert_eq!(encode_latin1("\u{100}"), None);
        assert_eq!(encode_latin1("a\u{1f600}"), None);
    }

    #[test]
    fn requests_before_ownership_are_refused() {
        assert!(request_in_time(100, 0));
        assert!(request_in_time(100, 100));
        assert!(request_in_time(100, 101));
        assert!(!request_in_time(100, 99));
        assert!(request_in_time(u32::MAX, 3));
        assert!(!request_in_time(3, u32::MAX));
    }

    #[test]
    fn pixels_are_premultiplied() {
        let rgba = [255, 128, 0, 255, 255, 255, 255, 0, 200, 100, 50, 128];
        let out = premultiplied_argb(&rgba, false);
        let words: Vec<u32> =
            out.chunks_exact(4).map(|chunk| u32::from_ne_bytes(chunk.try_into().unwrap())).collect();
        assert_eq!(words, vec![0xffff_8000, 0, 0x8064_3219]);
        let swapped = premultiplied_argb(&rgba[..4], true);
        assert_eq!(u32::from_ne_bytes(swapped.try_into().unwrap()), 0x0080_ffff);
        assert!(premultiplied_argb(&[1, 2, 3], false).is_empty());
    }

    #[test]
    fn runs_cover_visible_pixels() {
        let o = [0, 0, 0, 255];
        let t = [0, 0, 0, 0];
        let image: Vec<u8> = [o, o, t, o, t, t, t, t, o, o, o, o].concat();
        assert_eq!(opaque_runs(&image, 4), vec![
            Run { x: 0, y: 0, width: 2 },
            Run { x: 3, y: 0, width: 1 },
            Run { x: 0, y: 2, width: 4 },
        ]);
        assert!(opaque_runs(&image, 0).is_empty());
        assert!(opaque_runs(&[], 4).is_empty());
    }
}
