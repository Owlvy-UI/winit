//! Drag and drop state, without a virtual machine.
//!
//! The Java listener reports what the view hierarchy sees and the event loop asks what the
//! application wants. Both meet here: [`Dnd`] turns the reports into window events and holds the
//! answers until the next report needs them.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::ops::ControlFlow;
use std::sync::Arc;

use dpi::PhysicalPosition;
use winit_core::data_transfer::{
    DataTransfer, DataTransferId, DataTransferSend, SendData, TransferType, TypeHint, TypedData,
};
use winit_core::event::WindowEvent;
use winit_core::event_loop::{AsyncRequestSerial, DndAction};

/// The code of [`DndAction::Move`] shared with the Java listener.
pub(crate) const ACTION_MOVE: i32 = 1;

/// The code of [`DndAction::Copy`] shared with the Java listener.
pub(crate) const ACTION_COPY: i32 = 2;

/// The code of no action shared with the Java listener.
pub(crate) const NO_ACTION: i32 = 0;

/// The most action codes read from a drag description.
pub(crate) const MAX_ACTIONS: usize = 8;

/// The most MIME types read from a drag description.
pub(crate) const MAX_MIME_TYPES: usize = 64;

/// The most items read from or written to a clip.
pub(crate) const MAX_ITEMS: usize = 256;

/// The most bytes of text read from or written to a clip.
pub(crate) const MAX_BYTES: usize = 32 * 1024 * 1024;

/// The most fetches waiting for the drop of one transfer.
const MAX_FETCHES: usize = 64;

/// The most events waiting for the event loop.
const MAX_EVENTS: usize = 1024;

/// The longest side of a drag shadow in pixels.
pub(crate) const MAX_SHADOW_SIDE: u32 = 1024;

/// The MIME type of plain text.
const MIME_TEXT: &str = "text/plain";

/// The MIME type of HTML.
const MIME_HTML: &str = "text/html";

/// The MIME type of a URI list.
const MIME_URI_LIST: &str = "text/uri-list";

/// The MIME type of an intent item.
const MIME_INTENT: &str = "text/vnd.android.intent";

/// The action of a code shared with the Java listener.
pub(crate) fn action_of(code: i32) -> Option<DndAction> {
    match code {
        ACTION_MOVE => Some(DndAction::Move),
        ACTION_COPY => Some(DndAction::Copy),
        _ => None,
    }
}

/// The code shared with the Java listener for an action.
pub(crate) fn code_of(action: Option<DndAction>) -> i32 {
    match action {
        Some(DndAction::Move) => ACTION_MOVE,
        Some(DndAction::Copy) => ACTION_COPY,
        _ => NO_ACTION,
    }
}

/// The actions a source offers, from the codes in its drag description.
///
/// A source without codes is not a winit source and offers copy.
fn source_actions(codes: &[i32]) -> Vec<DndAction> {
    let mut actions = Vec::new();
    for action in codes.iter().take(MAX_ACTIONS).copied().filter_map(action_of) {
        if !actions.contains(&action) {
            actions.push(action);
        }
    }

    if actions.is_empty() {
        actions.push(DndAction::Copy);
    }

    actions
}

/// The first action of the target's list that the source offers.
fn accepted_action(valid: &[DndAction], source: &[DndAction]) -> Option<DndAction> {
    valid
        .iter()
        .copied()
        .filter(|action| matches!(action, DndAction::Copy | DndAction::Move))
        .find(|action| source.contains(action))
}

/// A MIME type of a clip, implementing [`TransferType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClipType {
    mime: Arc<str>,
    hint: Option<TypeHint>,
}

impl ClipType {
    fn new(mime: &str) -> Self {
        let hint = match mime {
            MIME_TEXT => Some(TypeHint::Plaintext),
            MIME_HTML => Some(TypeHint::Html),
            MIME_URI_LIST => Some(TypeHint::UriList),
            _ => None,
        };

        Self { mime: mime.into(), hint }
    }

    /// Whether items of this type carry a URI.
    fn carries_uri(&self) -> bool {
        self.hint != Some(TypeHint::Plaintext) && self.hint != Some(TypeHint::Html)
    }
}

impl TransferType for ClipType {
    fn hint(&self) -> Option<TypeHint> {
        self.hint
    }

    fn matches(&self, other: &dyn TransferType) -> bool {
        match other.cast_ref::<Self>() {
            Some(other) => self.mime == other.mime,
            None => self.hint.is_some_and(|hint| other.hint().is_some_and(|o| hint.matches(&o))),
        }
    }
}

/// The types of a drag description.
///
/// Every MIME type but text and HTML stands for items carrying a URI, so a URI list is offered
/// as soon as one of them is present.
fn clip_types(mimes: &[String]) -> Vec<ClipType> {
    let mut types: Vec<ClipType> = Vec::new();
    for mime in mimes.iter().take(MAX_MIME_TYPES) {
        let mime = mime.trim().to_ascii_lowercase();
        if mime.is_empty() || mime == MIME_INTENT || types.iter().any(|ty| *ty.mime == *mime) {
            continue;
        }

        types.push(ClipType::new(&mime));
    }

    let uri_list = types.iter().any(|ty| ty.hint == Some(TypeHint::UriList));
    if !uri_list && types.iter().any(ClipType::carries_uri) {
        types.push(ClipType::new(MIME_URI_LIST));
    }

    types
}

/// The types offered by an incoming drag, implementing [`DataTransfer`].
#[derive(Debug)]
struct ClipTransfer {
    types: Arc<[ClipType]>,
}

impl DataTransfer for ClipTransfer {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> ControlFlow<()>,
    ) {
        for ty in self.types.iter() {
            if func(ty).is_break() {
                break;
            }
        }
    }
}

/// What the items of a dropped clip carry.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ClipContent {
    pub(crate) texts: Vec<String>,
    pub(crate) htmls: Vec<String>,
    pub(crate) uris: Vec<String>,
}

/// The body of one fetched type.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Body {
    Text(String),
    Uris(Vec<String>),
}

/// One type of a dropped clip, implementing [`TypedData`].
#[derive(Debug)]
struct ClipValue {
    type_: ClipType,
    body: Body,
}

impl ClipValue {
    fn new(type_: ClipType, content: &ClipContent) -> Self {
        let body = match type_.hint {
            Some(TypeHint::Plaintext) => Body::Text(content.texts.join("\n")),
            Some(TypeHint::Html) => Body::Text(content.htmls.join("\n")),
            _ => Body::Uris(content.uris.clone()),
        };

        Self { type_, body }
    }
}

impl TypedData for ClipValue {
    fn type_(&self) -> &dyn TransferType {
        &self.type_
    }

    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        let bytes = match &self.body {
            Body::Text(text) => text.as_bytes().to_vec(),
            Body::Uris(uris) => uris.iter().flat_map(|uri| [uri.as_str(), "\r\n"]).collect::<String>().into_bytes(),
        };

        Some(Box::new(io::Cursor::new(bytes)))
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        match &self.body {
            Body::Uris(uris) => Ok(uris.clone()),
            Body::Text(_) => Err(io::ErrorKind::InvalidData.into()),
        }
    }

    fn try_as_string(&self) -> io::Result<String> {
        match &self.body {
            Body::Text(text) => Ok(text.clone()),
            Body::Uris(_) => Err(io::ErrorKind::InvalidData.into()),
        }
    }
}

/// An incoming drag over the window.
#[derive(Debug)]
struct Incoming {
    id: DataTransferId,
    types: Arc<[ClipType]>,
    source: Vec<DndAction>,
    valid: Vec<DndAction>,
    content: Option<ClipContent>,
    fetches: Vec<(AsyncRequestSerial, ClipType)>,
    dropped: bool,
}

/// What a request about a data transfer could not do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum DndError {
    /// The transfer is not the current incoming drag.
    UnknownTransfer,
    /// The drag does not offer the requested type.
    UnknownType,
    /// Too many fetches wait for the drop.
    TooManyFetches,
    /// Neither copy nor move is among the actions of an outgoing drag.
    NoAction,
    /// The outgoing data holds no text, HTML or URI list.
    NoData,
    /// The outgoing data holds a `file:` URI, which Android refuses to share.
    FileUri,
    /// The outgoing data exceeds the item or size limit.
    TooLarge,
}

impl DndError {
    /// A static description, as `NotSupportedError` takes it.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::UnknownTransfer => "the data transfer is not the current drag",
            Self::UnknownType => "the drag does not offer the requested type",
            Self::TooManyFetches => "too many fetches wait for the drop",
            Self::NoAction => "a drag on Android needs copy or move among its actions",
            Self::NoData => "a drag on Android needs text, HTML or a URI list",
            Self::FileUri => "Android refuses to share file URIs; use content URIs",
            Self::TooLarge => "the drag data exceeds the item or size limit",
        }
    }
}

impl fmt::Display for DndError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for DndError {}

/// The data of an outgoing drag as the Java listener takes it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct OutgoingClip {
    pub(crate) text: Option<String>,
    pub(crate) html: Option<String>,
    pub(crate) uris: Vec<String>,
    pub(crate) actions: Vec<i32>,
}

/// The text of an outgoing type.
fn send_text(data: SendData) -> Option<String> {
    match data {
        SendData::String(text) => Some(text),
        SendData::Bytes(bytes) => String::from_utf8(bytes).ok(),
        _ => None,
    }
}

/// Collects text, HTML and URIs of an outgoing drag.
///
/// # Errors
///
/// Returns [`DndError::NoAction`] without copy or move, [`DndError::NoData`] without a type
/// Android can carry, [`DndError::FileUri`] for a `file:` URI and [`DndError::TooLarge`] past the
/// limits.
pub(crate) fn outgoing_clip(
    send: &dyn DataTransferSend,
    actions: &[DndAction],
) -> Result<OutgoingClip, DndError> {
    let mut clip = OutgoingClip::default();
    for action in actions.iter().copied().filter(|a| matches!(a, DndAction::Copy | DndAction::Move))
    {
        let code = code_of(Some(action));
        if !clip.actions.contains(&code) {
            clip.actions.push(code);
        }
    }

    if clip.actions.is_empty() {
        return Err(DndError::NoAction);
    }

    for ty in send.available_types() {
        match ty.hint() {
            Some(TypeHint::Plaintext) if clip.text.is_none() => {
                clip.text = send.data_for_type(ty).and_then(send_text);
            },
            Some(TypeHint::Html) if clip.html.is_none() => {
                clip.html = send.data_for_type(ty).and_then(send_text);
            },
            Some(TypeHint::UriList) if clip.uris.is_empty() => {
                if let Some(SendData::Uris(uris)) = send.data_for_type(ty) {
                    clip.uris = uris;
                }
            },
            _ => {},
        }
    }

    if clip.text.is_none() && clip.html.is_none() && clip.uris.is_empty() {
        return Err(DndError::NoData);
    }

    if clip.uris.iter().any(|uri| uri.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("file:"))) {
        return Err(DndError::FileUri);
    }

    let bytes = clip.uris.iter().map(String::len).fold(0_usize, usize::saturating_add);
    let bytes = [&clip.text, &clip.html]
        .into_iter()
        .flatten()
        .map(String::len)
        .fold(bytes, usize::saturating_add);
    if clip.uris.len() > MAX_ITEMS || bytes > MAX_BYTES {
        return Err(DndError::TooLarge);
    }

    Ok(clip)
}

/// The pixels and touch point of a drag shadow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shadow {
    pub(crate) argb: Vec<i32>,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) touch_x: i32,
    pub(crate) touch_y: i32,
}

/// Converts RGBA pixels to the ARGB colors of an Android bitmap.
///
/// The icon is placed at the pointer plus the offset, so the touch point in the shadow is the
/// negated offset, kept inside the shadow. Returns `None` for an empty icon, a side longer than
/// [`MAX_SHADOW_SIDE`] or a buffer that does not match the size.
pub(crate) fn shadow(
    rgba: &[u8],
    width: u32,
    height: u32,
    offset_x: i32,
    offset_y: i32,
) -> Option<Shadow> {
    if width == 0 || height == 0 || width > MAX_SHADOW_SIDE || height > MAX_SHADOW_SIDE {
        return None;
    }

    let pixels = usize::try_from(width).ok()?.checked_mul(usize::try_from(height).ok()?)?;
    if rgba.len() != pixels.checked_mul(4)? {
        return None;
    }

    let argb = rgba
        .chunks_exact(4)
        .map(|pixel| match *pixel {
            [r, g, b, a] => i32::from_be_bytes([a, r, g, b]),
            _ => 0,
        })
        .collect();
    let width = i32::try_from(width).ok()?;
    let height = i32::try_from(height).ok()?;
    let touch = |offset: i32, side: i32| offset.saturating_neg().clamp(0, side.saturating_sub(1));

    Some(Shadow {
        argb,
        width,
        height,
        touch_x: touch(offset_x, width),
        touch_y: touch(offset_y, height),
    })
}

/// Drag and drop state of the window.
#[derive(Debug, Default)]
pub(crate) struct Dnd {
    next_id: i64,
    incoming: Option<Incoming>,
    outgoing: Option<DataTransferId>,
    events: VecDeque<WindowEvent>,
}

impl Dnd {
    fn next_id(&mut self) -> DataTransferId {
        let id = DataTransferId::from_raw(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    fn push(&mut self, event: WindowEvent) {
        if let WindowEvent::DragPosition { id, .. } = &event {
            if let Some(WindowEvent::DragPosition { id: last, .. }) = self.events.back() {
                if last == id {
                    self.events.pop_back();
                }
            }
        }

        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }

        self.events.push_back(event);
    }

    /// Takes the events waiting for the event loop.
    pub(crate) fn take_events(&mut self) -> VecDeque<WindowEvent> {
        std::mem::take(&mut self.events)
    }

    /// Whether events wait for the event loop.
    pub(crate) fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    fn live(&mut self) -> Option<&mut Incoming> {
        self.incoming.as_mut().filter(|incoming| !incoming.dropped)
    }

    /// Leaves an incoming drag that was not dropped.
    fn leave(&mut self) {
        if let Some(incoming) = self.incoming.take() {
            if incoming.dropped {
                self.incoming = Some(incoming);
            } else {
                self.push(WindowEvent::DragLeft { id: incoming.id });
            }
        }
    }

    /// A drag entered the window, with the MIME types and action codes of its description.
    pub(crate) fn entered(&mut self, mimes: &[String], codes: &[i32]) {
        self.leave();
        let id = self.next_id();
        self.incoming = Some(Incoming {
            id,
            types: clip_types(mimes).into(),
            source: source_actions(codes),
            valid: Vec::new(),
            content: None,
            fetches: Vec::new(),
            dropped: false,
        });
        self.push(WindowEvent::DragEntered { id, position: None });
    }

    /// The drag moved to a point of the window.
    pub(crate) fn located(&mut self, x: f32, y: f32) {
        let Some(incoming) = self.live() else { return };
        let id = incoming.id;
        let proposed_action = accepted_action(&incoming.valid, &incoming.source);
        let position = PhysicalPosition::new(f64::from(x), f64::from(y));
        self.push(WindowEvent::DragPosition { id, position, proposed_action });
    }

    /// The drag left the window.
    pub(crate) fn exited(&mut self) {
        self.leave();
    }

    /// The drag was released over the window, carrying `content`.
    ///
    /// Returns the code of the accepted action, or [`NO_ACTION`] for a rejected drop.
    pub(crate) fn dropped(&mut self, x: f32, y: f32, content: ClipContent) -> i32 {
        self.located(x, y);
        let Some(incoming) = self.live() else { return NO_ACTION };
        let Some(action) = accepted_action(&incoming.valid, &incoming.source) else {
            self.leave();
            return NO_ACTION;
        };

        incoming.dropped = true;
        let id = incoming.id;
        let fetches = std::mem::take(&mut incoming.fetches);
        self.push(WindowEvent::DragDropped { id, proposed_action: Some(action) });
        for (serial, type_) in fetches {
            let value: Arc<dyn TypedData> = Arc::new(ClipValue::new(type_, &content));
            self.push(WindowEvent::DataTransferReceived { id, serial, value });
        }

        if let Some(incoming) = self.incoming.as_mut() {
            incoming.content = Some(content);
        }

        code_of(Some(action))
    }

    /// A drag ended. `ours` marks the drag `drag` started by [`Dnd::start`], `result` whether a
    /// target accepted it and `code` the action a winit target answered.
    pub(crate) fn ended(&mut self, ours: bool, drag: i64, result: bool, code: i32) {
        self.leave();
        if !ours {
            return;
        }

        let Some(id) = self.outgoing.filter(|id| id.into_raw() == drag) else { return };
        self.outgoing = None;
        if result {
            let action = action_of(code).or(Some(DndAction::Copy));
            self.push(WindowEvent::OutgoingDragDropped { id, action });
        } else {
            self.push(WindowEvent::OutgoingDragCanceled { id });
        }
    }

    /// The outgoing drag `drag` could not be started.
    pub(crate) fn failed(&mut self, drag: i64) {
        if let Some(id) = self.outgoing.filter(|id| id.into_raw() == drag) {
            self.outgoing = None;
            self.push(WindowEvent::OutgoingDragCanceled { id });
        }
    }

    /// Starts an outgoing drag, canceling one that has not ended.
    pub(crate) fn start(&mut self) -> DataTransferId {
        if let Some(id) = self.outgoing.take() {
            self.push(WindowEvent::OutgoingDragCanceled { id });
        }

        let id = self.next_id();
        self.outgoing = Some(id);
        id
    }

    /// Forgets the outgoing drag `drag` without an event, for a start that was refused.
    pub(crate) fn abandon(&mut self, drag: DataTransferId) {
        if self.outgoing == Some(drag) {
            self.outgoing = None;
        }
    }

    fn incoming(&mut self, id: DataTransferId) -> Result<&mut Incoming, DndError> {
        self.incoming.as_mut().filter(|incoming| incoming.id == id).ok_or(DndError::UnknownTransfer)
    }

    /// The types offered by the incoming drag `id`.
    ///
    /// # Errors
    ///
    /// Returns [`DndError::UnknownTransfer`] when `id` is not the current incoming drag.
    pub(crate) fn data_transfer(
        &mut self,
        id: DataTransferId,
    ) -> Result<Box<dyn DataTransfer>, DndError> {
        let types = Arc::clone(&self.incoming(id)?.types);
        Ok(Box::new(ClipTransfer { types }))
    }

    /// Sets the actions the application accepts for the incoming drag `id`.
    ///
    /// # Errors
    ///
    /// Returns [`DndError::UnknownTransfer`] when `id` is not the current incoming drag.
    pub(crate) fn set_valid(&mut self, id: DataTransferId, actions: &[DndAction]) -> Result<(), DndError> {
        let incoming = self.incoming(id)?;
        if !incoming.dropped {
            incoming.valid = actions.iter().copied().take(MAX_ACTIONS).collect();
        }

        Ok(())
    }

    /// Requests one type of the incoming drag `id`. Before the drop the request waits for it.
    ///
    /// # Errors
    ///
    /// Returns [`DndError::UnknownTransfer`] for an unknown drag, [`DndError::UnknownType`] for a
    /// type it does not offer and [`DndError::TooManyFetches`] past the limit of waiting fetches.
    pub(crate) fn fetch(
        &mut self,
        id: DataTransferId,
        type_: &dyn TransferType,
    ) -> Result<AsyncRequestSerial, DndError> {
        let incoming = self.incoming(id)?;
        let type_ = incoming
            .types
            .iter()
            .find(|ty| ty.matches(type_))
            .cloned()
            .ok_or(DndError::UnknownType)?;
        if incoming.content.is_none() && incoming.fetches.len() >= MAX_FETCHES {
            return Err(DndError::TooManyFetches);
        }

        let serial = AsyncRequestSerial::get();
        match &incoming.content {
            Some(content) => {
                let value: Arc<dyn TypedData> = Arc::new(ClipValue::new(type_, content));
                self.push(WindowEvent::DataTransferReceived { id, serial, value });
            },
            None => incoming.fetches.push((serial, type_)),
        }

        Ok(serial)
    }
}

#[cfg(test)]
mod tests {
    use winit_core::data_transfer::DataTransferSendBuilder;

    use super::*;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|mime| (*mime).to_owned()).collect()
    }

    fn entered(dnd: &mut Dnd, list: &[&str], codes: &[i32]) -> DataTransferId {
        dnd.entered(&mimes(list), codes);
        match dnd.take_events().pop_back() {
            Some(WindowEvent::DragEntered { id, position: None }) => id,
            other => panic!("expected DragEntered, got {other:?}"),
        }
    }

    fn content() -> ClipContent {
        ClipContent {
            texts: vec!["hello".to_owned(), "world".to_owned()],
            htmls: vec!["<b>hello</b>".to_owned()],
            uris: vec!["content://a/1".to_owned(), "content://a/2".to_owned()],
        }
    }

    #[test]
    fn codes_round_trip() {
        assert_eq!(action_of(code_of(Some(DndAction::Move))), Some(DndAction::Move));
        assert_eq!(action_of(code_of(Some(DndAction::Copy))), Some(DndAction::Copy));
        assert_eq!(code_of(Some(DndAction::Link)), NO_ACTION);
        assert_eq!(code_of(None), NO_ACTION);
        assert_eq!(action_of(NO_ACTION), None);
        assert_eq!(action_of(-1), None);
        assert_eq!(action_of(i32::MAX), None);
    }

    #[test]
    fn a_foreign_source_offers_copy() {
        assert_eq!(source_actions(&[]), vec![DndAction::Copy]);
        assert_eq!(source_actions(&[7, -3]), vec![DndAction::Copy]);
    }

    #[test]
    fn source_actions_keep_order_and_drop_repeats() {
        assert_eq!(
            source_actions(&[ACTION_MOVE, ACTION_COPY, ACTION_MOVE]),
            vec![DndAction::Move, DndAction::Copy]
        );
        let many = [ACTION_COPY; 100];
        assert_eq!(source_actions(&many), vec![DndAction::Copy]);
    }

    #[test]
    fn the_target_preference_decides() {
        let both = [DndAction::Move, DndAction::Copy];
        assert_eq!(accepted_action(&[DndAction::Move, DndAction::Copy], &both), Some(DndAction::Move));
        assert_eq!(accepted_action(&[DndAction::Copy, DndAction::Move], &both), Some(DndAction::Copy));
        assert_eq!(accepted_action(&[DndAction::Move], &[DndAction::Copy]), None);
        assert_eq!(accepted_action(&[DndAction::Link, DndAction::Ask], &both), None);
        assert_eq!(accepted_action(&[], &both), None);
    }

    #[test]
    fn types_follow_the_description() {
        let types = clip_types(&mimes(&["text/plain", "TEXT/HTML ", "text/plain", ""]));
        let hints: Vec<_> = types.iter().map(|ty| ty.hint).collect();
        assert_eq!(hints, vec![Some(TypeHint::Plaintext), Some(TypeHint::Html)]);
    }

    #[test]
    fn a_content_type_offers_a_uri_list() {
        let types = clip_types(&mimes(&["image/png", "text/vnd.android.intent"]));
        let spelled: Vec<_> = types.iter().map(|ty| &*ty.mime).collect();
        assert_eq!(spelled, vec!["image/png", "text/uri-list"]);
        assert!(types.iter().any(|ty| ty.hint == Some(TypeHint::UriList)));

        let types = clip_types(&mimes(&["text/uri-list", "image/png"]));
        assert_eq!(types.len(), 2);
    }

    #[test]
    fn mime_types_are_limited() {
        let many: Vec<String> = (0..1000).map(|n| format!("application/x-{n}")).collect();
        assert_eq!(clip_types(&many).len(), MAX_MIME_TYPES + 1);
        assert!(clip_types(&[]).is_empty());
    }

    #[test]
    fn a_drag_is_rejected_until_actions_are_set() {
        let mut dnd = Dnd::default();
        let id = entered(&mut dnd, &["text/plain"], &[]);
        dnd.located(1.0, 2.0);
        match dnd.take_events().pop_back() {
            Some(WindowEvent::DragPosition { id: got, position, proposed_action: None }) => {
                assert_eq!(got, id);
                assert_eq!(position, PhysicalPosition::new(1.0, 2.0));
            },
            other => panic!("unexpected {other:?}"),
        }

        assert_eq!(dnd.dropped(1.0, 2.0, content()), NO_ACTION);
        let events = dnd.take_events();
        assert!(matches!(events.back(), Some(WindowEvent::DragLeft { id: left }) if *left == id));
        assert!(dnd.data_transfer(id).is_err());
    }

    #[test]
    fn a_foreign_drag_drops_as_copy() {
        let mut dnd = Dnd::default();
        let id = entered(&mut dnd, &["text/plain"], &[]);
        dnd.set_valid(id, &[DndAction::Move, DndAction::Copy]).unwrap();
        dnd.located(3.0, 4.0);
        assert!(matches!(
            dnd.take_events().pop_back(),
            Some(WindowEvent::DragPosition { proposed_action: Some(DndAction::Copy), .. })
        ));
        assert_eq!(dnd.dropped(3.0, 4.0, content()), ACTION_COPY);
        assert!(matches!(
            dnd.take_events().pop_back(),
            Some(WindowEvent::DragDropped { proposed_action: Some(DndAction::Copy), .. })
        ));
    }

    #[test]
    fn a_winit_drag_drops_as_move() {
        let mut dnd = Dnd::default();
        let id = entered(&mut dnd, &["text/plain"], &[ACTION_COPY, ACTION_MOVE]);
        dnd.set_valid(id, &[DndAction::Move]).unwrap();
        assert_eq!(dnd.dropped(0.0, 0.0, content()), ACTION_MOVE);
    }

    #[test]
    fn positions_are_coalesced() {
        let mut dnd = Dnd::default();
        entered(&mut dnd, &["text/plain"], &[]);
        for step in 0..10_000_u16 {
            dnd.located(f32::from(step), 0.0);
        }

        let events = dnd.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events.front(),
            Some(WindowEvent::DragPosition { position, .. }) if position.x > 9998.0
        ));
    }

    #[test]
    fn fetches_before_the_drop_are_answered_after_it() {
        let mut dnd = Dnd::default();
        let id = entered(&mut dnd, &["text/plain", "text/html", "image/png"], &[]);
        dnd.set_valid(id, &[DndAction::Copy]).unwrap();
        let text = dnd.fetch(id, &TypeHint::Plaintext).unwrap();
        let uris = dnd.fetch(id, &TypeHint::UriList).unwrap();
        assert_eq!(dnd.fetch(id, &TypeHint::Rtf), Err(DndError::UnknownType));
        assert!(!dnd.has_events());

        dnd.dropped(5.0, 6.0, content());
        let events: Vec<_> = dnd.take_events().into_iter().collect();
        let [
            WindowEvent::DragPosition { .. },
            WindowEvent::DragDropped { .. },
            WindowEvent::DataTransferReceived { serial: first, value: text_value, .. },
            WindowEvent::DataTransferReceived { serial: second, value: uri_value, .. },
        ] = events.as_slice()
        else {
            panic!("unexpected {events:?}");
        };
        assert_eq!((*first, *second), (text, uris));
        assert_eq!(text_value.try_as_string().unwrap(), "hello\nworld");
        assert!(text_value.try_as_uris().is_err());
        assert_eq!(uri_value.try_as_uris().unwrap(), vec!["content://a/1", "content://a/2"]);
        assert_eq!(uri_value.try_as_bytes().unwrap(), b"content://a/1\r\ncontent://a/2\r\n");
        assert!(uri_value.try_as_string().is_err());

        let html = dnd.fetch(id, &TypeHint::Html).unwrap();
        match dnd.take_events().pop_back() {
            Some(WindowEvent::DataTransferReceived { serial, value, .. }) => {
                assert_eq!(serial, html);
                assert_eq!(value.try_as_string().unwrap(), "<b>hello</b>");
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn waiting_fetches_are_limited() {
        let mut dnd = Dnd::default();
        let id = entered(&mut dnd, &["text/plain"], &[]);
        for _ in 0..MAX_FETCHES {
            dnd.fetch(id, &TypeHint::Plaintext).unwrap();
        }

        assert_eq!(dnd.fetch(id, &TypeHint::Plaintext), Err(DndError::TooManyFetches));
    }

    #[test]
    fn requests_for_an_old_drag_fail() {
        let mut dnd = Dnd::default();
        let old = entered(&mut dnd, &["text/plain"], &[]);
        dnd.exited();
        assert!(matches!(dnd.take_events().pop_back(), Some(WindowEvent::DragLeft { .. })));
        let new = entered(&mut dnd, &["text/plain"], &[]);
        assert_ne!(old, new);
        assert_eq!(dnd.set_valid(old, &[DndAction::Copy]), Err(DndError::UnknownTransfer));
        assert_eq!(dnd.fetch(old, &TypeHint::Plaintext), Err(DndError::UnknownTransfer));
        assert!(dnd.set_valid(new, &[DndAction::Copy]).is_ok());
    }

    #[test]
    fn an_ended_drag_leaves_but_a_dropped_one_stays() {
        let mut dnd = Dnd::default();
        entered(&mut dnd, &["text/plain"], &[]);
        dnd.ended(false, 0, false, NO_ACTION);
        assert!(matches!(dnd.take_events().pop_back(), Some(WindowEvent::DragLeft { .. })));

        let id = entered(&mut dnd, &["text/plain"], &[]);
        dnd.set_valid(id, &[DndAction::Copy]).unwrap();
        dnd.dropped(0.0, 0.0, content());
        dnd.take_events();
        dnd.ended(false, 0, true, NO_ACTION);
        assert!(!dnd.has_events());
        assert!(dnd.data_transfer(id).is_ok());
    }

    #[test]
    fn outgoing_drags_report_the_answer() {
        let mut dnd = Dnd::default();
        let id = dnd.start();
        dnd.ended(true, id.into_raw(), true, ACTION_MOVE);
        assert!(matches!(
            dnd.take_events().pop_back(),
            Some(WindowEvent::OutgoingDragDropped { id: got, action: Some(DndAction::Move) }) if got == id
        ));

        let id = dnd.start();
        dnd.ended(true, id.into_raw(), true, NO_ACTION);
        assert!(matches!(
            dnd.take_events().pop_back(),
            Some(WindowEvent::OutgoingDragDropped { action: Some(DndAction::Copy), .. })
        ));

        let id = dnd.start();
        dnd.ended(true, id.into_raw(), false, ACTION_MOVE);
        assert!(matches!(
            dnd.take_events().pop_back(),
            Some(WindowEvent::OutgoingDragCanceled { id: got }) if got == id
        ));
    }

    #[test]
    fn a_failed_or_replaced_drag_is_canceled_once() {
        let mut dnd = Dnd::default();
        let first = dnd.start();
        let second = dnd.start();
        assert!(matches!(
            dnd.take_events().pop_back(),
            Some(WindowEvent::OutgoingDragCanceled { id }) if id == first
        ));
        dnd.ended(true, first.into_raw(), true, ACTION_COPY);
        assert!(!dnd.has_events());
        dnd.failed(second.into_raw());
        dnd.failed(second.into_raw());
        assert_eq!(dnd.take_events().len(), 1);

        let third = dnd.start();
        dnd.abandon(third);
        dnd.ended(true, third.into_raw(), true, ACTION_COPY);
        assert!(!dnd.has_events());
    }

    fn send(text: Option<&'static str>, uris: Option<Vec<String>>) -> Box<dyn DataTransferSend> {
        let mut builder = DataTransferSendBuilder::new(());
        if let Some(text) = text {
            builder.add_type(TypeHint::Plaintext, move |(), _| Some(text.to_owned()));
        }

        if let Some(uris) = uris {
            builder.add_type(TypeHint::UriList, move |(), _| Some(SendData::Uris(uris.clone())));
        }

        builder.add_type(TypeHint::Image { extension_hint: Some("png") }, |(), _| {
            Some(vec![1_u8, 2, 3])
        });
        builder.build()
    }

    #[test]
    fn outgoing_text_and_uris_are_collected() {
        let data = send(Some("hi"), Some(vec!["content://x/1".to_owned()]));
        let clip = outgoing_clip(&*data, &[DndAction::Move, DndAction::Link, DndAction::Copy, DndAction::Move])
            .unwrap();
        assert_eq!(clip.text.as_deref(), Some("hi"));
        assert_eq!(clip.html, None);
        assert_eq!(clip.uris, vec!["content://x/1"]);
        assert_eq!(clip.actions, vec![ACTION_MOVE, ACTION_COPY]);
    }

    #[test]
    fn outgoing_drags_are_checked() {
        let data = send(Some("hi"), None);
        assert_eq!(outgoing_clip(&*data, &[DndAction::Link]), Err(DndError::NoAction));
        assert_eq!(outgoing_clip(&*data, &[]), Err(DndError::NoAction));

        let data = send(None, None);
        assert_eq!(outgoing_clip(&*data, &[DndAction::Copy]), Err(DndError::NoData));

        let data = send(None, Some(vec!["FILE:///sdcard/a".to_owned()]));
        assert_eq!(outgoing_clip(&*data, &[DndAction::Copy]), Err(DndError::FileUri));

        let many = (0..=MAX_ITEMS).map(|n| format!("content://x/{n}")).collect();
        let data = send(None, Some(many));
        assert_eq!(outgoing_clip(&*data, &[DndAction::Copy]), Err(DndError::TooLarge));

        let data = send(None, Some(vec!["é".to_owned()]));
        assert!(outgoing_clip(&*data, &[DndAction::Copy]).is_ok());
    }

    #[test]
    fn shadows_are_argb_with_the_touch_inside() {
        let rgba = [1, 2, 3, 4, 5, 6, 7, 8];
        let made = shadow(&rgba, 2, 1, -1, -5).unwrap();
        assert_eq!(made.argb, vec![0x0401_0203, 0x0805_0607]);
        assert_eq!((made.width, made.height), (2, 1));
        assert_eq!((made.touch_x, made.touch_y), (1, 0));

        let white = [255_u8; 4];
        let made = shadow(&white, 1, 1, i32::MIN, 3).unwrap();
        assert_eq!(made.argb, vec![-1]);
        assert_eq!((made.touch_x, made.touch_y), (0, 0));
    }

    #[test]
    fn bad_shadows_are_refused() {
        assert_eq!(shadow(&[], 0, 0, 0, 0), None);
        assert_eq!(shadow(&[0; 4], 1, 2, 0, 0), None);
        assert_eq!(shadow(&[0; 3], 1, 1, 0, 0), None);
        assert_eq!(shadow(&[0; 4], MAX_SHADOW_SIDE + 1, 1, 0, 0), None);
        assert_eq!(shadow(&[0; 4], 1, u32::MAX, 0, 0), None);
    }
}
