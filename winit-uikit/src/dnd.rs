//! Drag and drop through `UIDropInteraction` and `UIDragInteraction`.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::OsString;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex};
use std::{fmt, fs, io};

use block2::{DynBlock, RcBlock};
use dispatch2::DispatchQueue;
use dpi::PhysicalPosition;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, MainThreadMarker};
use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{
    NSArray, NSData, NSError, NSItemProvider, NSItemProviderErrorDomain,
    NSItemProviderRepresentationVisibility, NSProgress, NSString, NSURL,
};
use objc2_ui_kit::{
    UIDragDropSession, UIDragItem, UIDragPreviewParameters, UIDragPreviewTarget, UIDropOperation,
    UIDropProposal, UIDropSession, UIImage, UIImageOrientation, UIImageView, UITargetedDragPreview,
    UIView,
};
use winit_core::data_transfer::{
    DataTransfer, DataTransferId, DataTransferSend, SendData, TransferType, TypeHint, TypedData,
};
use winit_core::error::{OsError, RequestError};
use winit_core::event::WindowEvent;
use winit_core::event_loop::{AsyncRequestSerial, DndAction, DragIcon};
use winit_core::icon::RgbaIcon;
use winit_core::window::WindowId;

use crate::app_state::{self, AppState, EventWrapper};

/// Largest representation copied out of a dropped item, in bytes.
pub(crate) const MAX_DATA_BYTES: usize = 64 * 1024 * 1024;

/// Largest file copied out of a dropped item, in bytes.
pub(crate) const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// Most items of one drag that are read or offered.
pub(crate) const MAX_ITEMS: usize = 1024;

/// Most type identifiers listed for one drag.
pub(crate) const MAX_TYPES: usize = 256;

/// `NSItemProviderItemUnavailableError`.
const ITEM_UNAVAILABLE: isize = -1000;

/// Type identifiers with a cross-platform meaning, in the order they are offered.
const KNOWN_TYPES: &[(&str, TypeHint)] = &[
    ("public.utf8-plain-text", TypeHint::Plaintext),
    ("public.plain-text", TypeHint::Plaintext),
    ("public.html", TypeHint::Html),
    ("public.rtf", TypeHint::Rtf),
    ("public.file-url", TypeHint::UriList),
    ("public.url", TypeHint::UriList),
    ("public.png", TypeHint::Image { extension_hint: Some("png") }),
    ("public.jpeg", TypeHint::Image { extension_hint: Some("jpeg") }),
    ("public.tiff", TypeHint::Image { extension_hint: Some("tiff") }),
    ("com.compuserve.gif", TypeHint::Image { extension_hint: Some("gif") }),
    ("public.heic", TypeHint::Image { extension_hint: Some("heic") }),
    ("public.mp3", TypeHint::Audio { extension_hint: Some("mp3") }),
    ("com.microsoft.waveform-audio", TypeHint::Audio { extension_hint: Some("wav") }),
    ("public.aiff-audio", TypeHint::Audio { extension_hint: Some("aiff") }),
    ("public.mpeg-4-audio", TypeHint::Audio { extension_hint: Some("m4a") }),
];

/// Abstract type identifiers that name a kind of data but no encoding.
const ABSTRACT_TYPES: &[(&str, TypeHint)] = &[
    ("public.image", TypeHint::Image { extension_hint: None }),
    ("public.audio", TypeHint::Audio { extension_hint: None }),
];

/// The type identifiers that carry a URI as UTF-8 text.
const URL_TYPES: &[&str] = &["public.file-url", "public.url"];

/// What went wrong in a drag and drop request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum DndError {
    /// No drag with this ID is known.
    UnknownTransfer(DataTransferId),
    /// The drag offers nothing of the requested type.
    NoSuchType(String),
    /// A representation is larger than the limit.
    TooLarge {
        /// The type identifier of the representation.
        uti: String,
        /// Its size in bytes.
        len: u64,
        /// The limit in bytes.
        limit: u64,
    },
    /// A representation that should hold text is not UTF-8.
    NotUtf8(String),
    /// A representation that should hold a URI holds none.
    NotAUri(String),
    /// The item provider failed to load a representation.
    Load {
        /// The type identifier that was loaded.
        uti: String,
        /// What the item provider reported.
        reason: String,
    },
    /// A dropped file could not be copied.
    Copy {
        /// The file that was copied.
        path: PathBuf,
        /// What the file system reported.
        reason: String,
    },
    /// No touch of the window is down, so no drag can be lifted.
    NoTouch(WindowId),
    /// Another outgoing drag is armed or running.
    Busy,
    /// None of the actions is copy or move.
    NoActions,
}

impl fmt::Display for DndError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTransfer(id) => {
                write!(f, "unknown data transfer with ID {}", id.into_raw())
            },
            Self::NoSuchType(uti) => write!(f, "the drag offers no data of type {uti}"),
            Self::TooLarge { uti, len, limit } => {
                write!(f, "{uti} holds {len} bytes, more than the limit of {limit}")
            },
            Self::NotUtf8(uti) => write!(f, "{uti} is not UTF-8"),
            Self::NotAUri(uti) => write!(f, "{uti} holds no URI"),
            Self::Load { uti, reason } => write!(f, "{uti} could not be loaded: {reason}"),
            Self::Copy { path, reason } => {
                write!(f, "{} could not be copied: {reason}", path.display())
            },
            Self::NoTouch(id) => write!(f, "no touch is down in window {}", id.into_raw()),
            Self::Busy => write!(f, "another outgoing drag is armed or running"),
            Self::NoActions => write!(f, "neither copy nor move is among the actions"),
        }
    }
}

impl std::error::Error for DndError {}

impl From<DndError> for RequestError {
    fn from(error: DndError) -> Self {
        Self::Os(OsError::new(line!(), file!(), error))
    }
}

/// A uniform type identifier, implementing [`TransferType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UtiType {
    hint: Option<TypeHint>,
    identifier: Arc<str>,
}

impl UtiType {
    fn new(identifier: &str) -> Self {
        Self { hint: hint_for_uti(identifier), identifier: identifier.into() }
    }
}

impl TransferType for UtiType {
    fn hint(&self) -> Option<TypeHint> {
        self.hint
    }

    fn matches(&self, other: &dyn TransferType) -> bool {
        if let Some(other) = other.cast_ref::<Self>() {
            self.identifier == other.identifier
        } else {
            self.hint.zip(other.hint()).is_some_and(|(this, other)| this.matches(&other))
        }
    }
}

/// The cross-platform meaning of a type identifier.
pub(crate) fn hint_for_uti(uti: &str) -> Option<TypeHint> {
    KNOWN_TYPES
        .iter()
        .chain(ABSTRACT_TYPES)
        .find(|(known, _)| *known == uti)
        .map(|(_, hint)| *hint)
}

/// The concrete type identifiers registered for an outgoing type hint.
pub(crate) fn utis_for_hint(hint: TypeHint) -> Vec<&'static str> {
    KNOWN_TYPES
        .iter()
        .filter(|(uti, known)| known.matches(&hint) && !URL_TYPES.contains(uti))
        .map(|(uti, _)| *uti)
        .collect()
}

/// The action a drop takes, the first of `valid` that the session allows.
///
/// Copy is always allowed. Move is allowed only when `move_allowed` holds.
pub(crate) fn drop_action(valid: &[DndAction], move_allowed: bool) -> Option<DndAction> {
    valid.iter().copied().find(|action| match action {
        DndAction::Copy => true,
        DndAction::Move => move_allowed,
        _ => false,
    })
}

/// The operation answered to `UIKit` for a drop action.
///
/// An empty list cancels, a list without an allowed action forbids.
pub(crate) fn operation_for(action: Option<DndAction>, valid: &[DndAction]) -> UIDropOperation {
    match action {
        Some(DndAction::Move) => UIDropOperation::Move,
        Some(_) => UIDropOperation::Copy,
        None if valid.is_empty() => UIDropOperation::Cancel,
        None => UIDropOperation::Forbidden,
    }
}

/// The action of an ended outgoing drag, `None` for a cancel or a forbidden drop.
pub(crate) fn action_for_operation(operation: UIDropOperation) -> Option<DndAction> {
    if operation == UIDropOperation::Copy {
        Some(DndAction::Copy)
    } else if operation == UIDropOperation::Move {
        Some(DndAction::Move)
    } else {
        None
    }
}

/// Whether an outgoing drag offers move.
pub(crate) fn offers_move(actions: &[DndAction]) -> bool {
    actions.contains(&DndAction::Move)
}

/// Reads the URI out of a URL representation.
pub(crate) fn uri_from_data(uti: &str, bytes: &[u8]) -> Result<String, DndError> {
    let text = std::str::from_utf8(bytes).map_err(|_| DndError::NotUtf8(uti.to_owned()))?;
    let uri = text.trim_end_matches('\0').trim();
    if uri.is_empty() || uri.contains(['\r', '\n']) {
        return Err(DndError::NotAUri(uti.to_owned()));
    }

    Ok(uri.to_owned())
}

/// Encodes URIs as `text/uri-list`.
pub(crate) fn uri_list_bytes(uris: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for uri in uris {
        out.extend_from_slice(uri.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    out
}

/// Refuses a representation larger than `limit`.
pub(crate) fn check_size(uti: &str, len: u64, limit: u64) -> Result<(), DndError> {
    if len > limit {
        return Err(DndError::TooLarge { uti: uti.to_owned(), len, limit });
    }

    Ok(())
}

/// The name a dropped file is copied under, the last component of its path.
pub(crate) fn copy_name(path: &Path) -> Option<OsString> {
    path.file_name().filter(|name| !name.is_empty()).map(ToOwned::to_owned)
}

/// The center of a drag preview of `size` points whose top left corner sits at `offset` image
/// pixels from `location`.
pub(crate) fn preview_center(
    location: CGPoint,
    offset: (i32, i32),
    size: CGSize,
    scale: f64,
) -> CGPoint {
    CGPoint {
        x: location.x + f64::from(offset.0) / scale + size.width / 2.0,
        y: location.y + f64::from(offset.1) / scale + size.height / 2.0,
    }
}

/// The type a fetch asks for.
#[derive(Debug, Clone)]
enum Wanted {
    Uti(UtiType),
    Hint(TypeHint),
}

impl Wanted {
    fn from_dyn(type_: &dyn TransferType) -> Option<Self> {
        match type_.cast_ref::<UtiType>() {
            Some(uti) => Some(Self::Uti(uti.clone())),
            None => type_.hint().map(Self::Hint),
        }
    }

    fn accepts(&self, uti: &str) -> bool {
        match self {
            Self::Uti(wanted) => *wanted.identifier == *uti,
            Self::Hint(hint) => hint_for_uti(uti).is_some_and(|known| known.matches(hint)),
        }
    }

    fn is_uri_list(&self) -> bool {
        match self {
            Self::Uti(_) => false,
            Self::Hint(hint) => *hint == TypeHint::UriList,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Uti(uti) => uti.identifier.to_string(),
            Self::Hint(hint) => format!("{hint:?}"),
        }
    }
}

/// The list of types of an incoming drag, implementing [`DataTransfer`].
#[derive(Debug)]
struct DropTypes {
    types: Vec<UtiType>,
}

impl DataTransfer for DropTypes {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> ControlFlow<()>,
    ) {
        for uti in &self.types {
            if func(uti).is_break() {
                break;
            }
        }
    }
}

/// The type a received value was asked for.
#[derive(Debug)]
enum ReceivedType {
    Uti(UtiType),
    Hint(TypeHint),
}

/// Data loaded out of a drop, implementing [`TypedData`].
#[derive(Debug)]
struct Received {
    type_: ReceivedType,
    content: Result<Content, DndError>,
}

#[derive(Debug)]
struct Content {
    bytes: Vec<u8>,
    uris: Option<Vec<String>>,
}

impl Received {
    fn content(&self) -> io::Result<&Content> {
        self.content.as_ref().map_err(|error| io::Error::other(error.clone()))
    }
}

impl TypedData for Received {
    fn type_(&self) -> &dyn TransferType {
        match &self.type_ {
            ReceivedType::Uti(uti) => uti,
            ReceivedType::Hint(hint) => hint,
        }
    }

    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        let bytes = self.content().ok()?.bytes.clone();
        Some(Box::new(io::Cursor::new(bytes)))
    }

    fn try_as_bytes(&self) -> io::Result<Vec<u8>> {
        Ok(self.content()?.bytes.clone())
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        self.content()?.uris.clone().ok_or_else(|| io::ErrorKind::InvalidData.into())
    }

    fn try_as_string(&self) -> io::Result<String> {
        String::from_utf8(self.content()?.bytes.clone())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

/// A fetch requested before the drop, started when the drop happens.
#[derive(Debug)]
struct PendingFetch {
    serial: AsyncRequestSerial,
    wanted: Wanted,
}

/// Where an incoming drag stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Over the view.
    Over,
    /// Left the view without a drop.
    Left,
    /// Dropped on the view.
    Dropped,
}

/// The incoming drag over a window.
#[derive(Debug)]
struct Incoming {
    id: DataTransferId,
    window_id: WindowId,
    providers: Vec<Retained<NSItemProvider>>,
    valid_actions: Vec<DndAction>,
    move_allowed: bool,
    local: bool,
    stage: Stage,
    pending: Vec<PendingFetch>,
}

/// An outgoing drag, armed by `start_drag` or lifted by `UIKit`.
enum Outgoing {
    Armed {
        id: DataTransferId,
        window_id: WindowId,
        data: Box<dyn DataTransferSend>,
        actions: Vec<DndAction>,
        icon: Option<DragIcon>,
    },
    Active {
        id: DataTransferId,
        window_id: WindowId,
        actions: Vec<DndAction>,
        icon: Option<DragIcon>,
    },
}

impl fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Armed { id, window_id, actions, .. } => f
                .debug_struct("Armed")
                .field("id", id)
                .field("window_id", window_id)
                .field("actions", actions)
                .finish_non_exhaustive(),
            Self::Active { id, window_id, actions, .. } => f
                .debug_struct("Active")
                .field("id", id)
                .field("window_id", window_id)
                .field("actions", actions)
                .finish_non_exhaustive(),
        }
    }
}

/// Drag and drop state of the application, used on the main thread only.
#[derive(Debug)]
pub(crate) struct DndState {
    next_id: Cell<i64>,
    incoming: RefCell<Option<Incoming>>,
    outgoing: RefCell<Option<Outgoing>>,
    touching: RefCell<HashSet<WindowId>>,
}

impl Default for DndState {
    fn default() -> Self {
        Self {
            next_id: Cell::new(1),
            incoming: RefCell::new(None),
            outgoing: RefCell::new(None),
            touching: RefCell::new(HashSet::new()),
        }
    }
}

impl DndState {
    fn next_id(&self) -> DataTransferId {
        let id = self.next_id.get();
        // Wraps after 2^63 drags.
        self.next_id.set(id.wrapping_add(1));
        DataTransferId::from_raw(id)
    }

    /// The types of the incoming drag `id`.
    pub(crate) fn data_transfer(&self, id: DataTransferId) -> Result<Box<dyn DataTransfer>, DndError> {
        let incoming = self.incoming.borrow();
        let incoming = incoming
            .as_ref()
            .filter(|incoming| incoming.id == id)
            .ok_or(DndError::UnknownTransfer(id))?;

        Ok(Box::new(DropTypes { types: registered_types(&incoming.providers) }))
    }

    /// Sets the actions the application accepts for the incoming drag `id`.
    pub(crate) fn set_valid_actions(
        &self,
        id: DataTransferId,
        actions: &[DndAction],
    ) -> Result<(), DndError> {
        let mut incoming = self.incoming.borrow_mut();
        let incoming = incoming
            .as_mut()
            .filter(|incoming| incoming.id == id && incoming.stage != Stage::Dropped)
            .ok_or(DndError::UnknownTransfer(id))?;
        incoming.valid_actions.clear();
        incoming.valid_actions.extend_from_slice(actions);

        Ok(())
    }

    /// Requests a type of the incoming drag `id`.
    ///
    /// Data of another application can only be loaded after the drop, so a fetch before the drop
    /// is started when the drop happens.
    pub(crate) fn fetch(
        &self,
        id: DataTransferId,
        type_: &dyn TransferType,
    ) -> Result<AsyncRequestSerial, DndError> {
        let mut incoming = self.incoming.borrow_mut();
        let incoming = incoming
            .as_mut()
            .filter(|incoming| incoming.id == id)
            .ok_or(DndError::UnknownTransfer(id))?;
        let wanted =
            Wanted::from_dyn(type_).ok_or_else(|| DndError::NoSuchType(format!("{type_:?}")))?;
        if !offers(&incoming.providers, &wanted) {
            return Err(DndError::NoSuchType(wanted.label()));
        }

        let serial = AsyncRequestSerial::get();
        if incoming.stage == Stage::Dropped || incoming.local {
            start_fetch(&incoming.providers, incoming.id, incoming.window_id, serial, &wanted);
        } else {
            incoming.pending.push(PendingFetch { serial, wanted });
        }

        Ok(serial)
    }

    /// Arms an outgoing drag for the next lift in `window_id`.
    pub(crate) fn arm(
        &self,
        window_id: WindowId,
        data: Box<dyn DataTransferSend>,
        actions: &[DndAction],
        icon: Option<DragIcon>,
    ) -> Result<DataTransferId, DndError> {
        if !actions.iter().any(|action| matches!(action, DndAction::Copy | DndAction::Move)) {
            return Err(DndError::NoActions);
        }

        if !self.touching.borrow().contains(&window_id) {
            return Err(DndError::NoTouch(window_id));
        }

        let mut outgoing = self.outgoing.borrow_mut();
        if matches!(*outgoing, Some(Outgoing::Active { .. })) {
            return Err(DndError::Busy);
        }

        let id = self.next_id();
        let previous = outgoing.replace(Outgoing::Armed {
            id,
            window_id,
            data,
            actions: actions.to_vec(),
            icon,
        });
        drop(outgoing);
        if let Some(Outgoing::Armed { id, window_id, .. }) = previous {
            emit(window_id, WindowEvent::OutgoingDragCanceled { id });
        }

        Ok(id)
    }

    /// Records whether a touch of `window_id` is down. The last touch ending without a lift
    /// cancels an armed drag.
    pub(crate) fn set_touching(&self, window_id: WindowId, touching: bool) {
        if touching {
            self.touching.borrow_mut().insert(window_id);
            return;
        }

        self.touching.borrow_mut().remove(&window_id);
        let mut outgoing = self.outgoing.borrow_mut();
        let armed_here =
            matches!(*outgoing, Some(Outgoing::Armed { window_id: armed, .. }) if armed == window_id);
        if !armed_here {
            return;
        }

        if let Some(Outgoing::Armed { id, .. }) = outgoing.take() {
            drop(outgoing);
            emit(window_id, WindowEvent::OutgoingDragCanceled { id });
        }
    }
}

/// Sends a window event to the application.
fn emit(window_id: WindowId, event: WindowEvent) {
    if let Some(mtm) = MainThreadMarker::new() {
        app_state::handle_nonuser_event(mtm, EventWrapper::Window { window_id, event });
    } else {
        tracing::error!("drag and drop event outside the main thread");
    }
}

fn state(mtm: MainThreadMarker) -> &'static DndState {
    AppState::get(mtm).dnd()
}

/// The item providers of a session, at most [`MAX_ITEMS`].
fn providers_of(session: &ProtocolObject<dyn UIDropSession>) -> Vec<Retained<NSItemProvider>> {
    session.items().iter().take(MAX_ITEMS).map(|item| item.itemProvider()).collect()
}

/// The type identifiers of all items, without duplicates, at most [`MAX_TYPES`].
fn registered_types(providers: &[Retained<NSItemProvider>]) -> Vec<UtiType> {
    let mut types: Vec<UtiType> = Vec::new();
    for provider in providers {
        let registered = provider.registeredTypeIdentifiers();
        for uti in &registered {
            if types.len() >= MAX_TYPES {
                return types;
            }

            let uti = uti.to_string();
            if !types.iter().any(|known| *known.identifier == *uti) {
                types.push(UtiType::new(&uti));
            }
        }
    }

    types
}

/// Whether any item offers the wanted type.
fn offers(providers: &[Retained<NSItemProvider>], wanted: &Wanted) -> bool {
    if wanted.is_uri_list() {
        return !providers.is_empty();
    }

    providers
        .iter()
        .any(|provider| provider.registeredTypeIdentifiers().iter().any(|uti| wanted.accepts(&uti.to_string())))
}

/// The location of a session in `view`, in physical pixels.
fn position(view: &UIView, session: &ProtocolObject<dyn UIDropSession>) -> PhysicalPosition<f64> {
    let location = session.locationInView(view);
    PhysicalPosition::from_logical::<(f64, f64), f64>(
        (location.x, location.y),
        view.contentScaleFactor(),
    )
}

/// A drag entered the view of `window_id`.
pub(crate) fn drop_entered(
    mtm: MainThreadMarker,
    window_id: WindowId,
    view: &UIView,
    session: &ProtocolObject<dyn UIDropSession>,
) {
    let state = state(mtm);
    let id = state.next_id();
    state.incoming.replace(Some(Incoming {
        id,
        window_id,
        providers: providers_of(session),
        valid_actions: Vec::new(),
        move_allowed: session.allowsMoveOperation() && session.localDragSession().is_some(),
        local: session.localDragSession().is_some(),
        stage: Stage::Over,
        pending: Vec::new(),
    }));

    emit(window_id, WindowEvent::DragEntered { id, position: Some(position(view, session)) });
}

/// The drag moved over the view. Returns the proposal for `UIKit`.
pub(crate) fn drop_updated(
    mtm: MainThreadMarker,
    view: &UIView,
    session: &ProtocolObject<dyn UIDropSession>,
) -> Retained<UIDropProposal> {
    let state = state(mtm);
    let current = state.incoming.borrow_mut().as_mut().filter(|incoming| incoming.stage == Stage::Over).map(
        |incoming| {
            incoming.providers = providers_of(session);
            (incoming.id, incoming.window_id)
        },
    );

    if let Some((id, window_id)) = current {
        let proposed_action = proposed(state);
        emit(window_id, WindowEvent::DragPosition {
            id,
            position: position(view, session),
            proposed_action,
        });
    }

    let operation = state
        .incoming
        .borrow()
        .as_ref()
        .filter(|incoming| incoming.stage == Stage::Over)
        .map_or(UIDropOperation::Cancel, |incoming| {
            operation_for(
                drop_action(&incoming.valid_actions, incoming.move_allowed),
                &incoming.valid_actions,
            )
        });

    UIDropProposal::initWithDropOperation(mtm.alloc(), operation)
}

/// The action the current answer of the application gives.
fn proposed(state: &DndState) -> Option<DndAction> {
    state
        .incoming
        .borrow()
        .as_ref()
        .and_then(|incoming| drop_action(&incoming.valid_actions, incoming.move_allowed))
}

/// The drag left the view without a drop.
pub(crate) fn drop_exited(mtm: MainThreadMarker) {
    let state = state(mtm);
    let left = state.incoming.borrow_mut().as_mut().filter(|incoming| incoming.stage == Stage::Over).map(
        |incoming| {
            incoming.stage = Stage::Left;
            (incoming.id, incoming.window_id)
        },
    );

    if let Some((id, window_id)) = left {
        emit(window_id, WindowEvent::DragLeft { id });
    }
}

/// The drag was dropped on the view.
pub(crate) fn drop_performed(
    mtm: MainThreadMarker,
    view: &UIView,
    session: &ProtocolObject<dyn UIDropSession>,
) {
    let state = state(mtm);
    let current = state.incoming.borrow_mut().as_mut().filter(|incoming| incoming.stage == Stage::Over).map(
        |incoming| {
            incoming.providers = providers_of(session);
            (incoming.id, incoming.window_id)
        },
    );
    let Some((id, window_id)) = current else {
        return;
    };

    emit(window_id, WindowEvent::DragPosition {
        id,
        position: position(view, session),
        proposed_action: proposed(state),
    });

    let proposed_action = proposed(state);
    if let Some(incoming) = state.incoming.borrow_mut().as_mut() {
        incoming.stage = Stage::Dropped;
        for fetch in std::mem::take(&mut incoming.pending) {
            start_fetch(&incoming.providers, id, window_id, fetch.serial, &fetch.wanted);
        }
    }

    emit(window_id, WindowEvent::DragDropped { id, proposed_action });
}

/// The drop session ended. A drag that neither left nor dropped is reported as left.
pub(crate) fn drop_ended(mtm: MainThreadMarker) {
    drop_exited(mtm);
}

/// Starts loading the wanted type and delivers it on the main thread.
fn start_fetch(
    providers: &[Retained<NSItemProvider>],
    id: DataTransferId,
    window_id: WindowId,
    serial: AsyncRequestSerial,
    wanted: &Wanted,
) {
    if wanted.is_uri_list() {
        fetch_uris(providers, id, window_id, serial);
        return;
    }

    let found = providers.iter().find_map(|provider| {
        provider
            .registeredTypeIdentifiers()
            .iter()
            .find(|uti| wanted.accepts(&uti.to_string()))
            .map(|uti| (provider, uti))
    });
    let received_type = match wanted {
        Wanted::Uti(uti) => ReceivedType::Uti(uti.clone()),
        Wanted::Hint(hint) => ReceivedType::Hint(*hint),
    };
    let Some((provider, uti)) = found else {
        deliver(id, window_id, serial, Received {
            type_: received_type,
            content: Err(DndError::NoSuchType(wanted.label())),
        });
        return;
    };

    let slot = Mutex::new(Some(received_type));
    let name = uti.to_string();
    load_data(provider, &uti, move |result| {
        let type_ = match slot.lock() {
            Ok(mut slot) => slot.take(),
            Err(_poisoned) => {
                tracing::warn!("the type slot of drop {} is poisoned; dropping the data", id.into_raw());
                None
            },
        };
        let Some(type_) = type_ else {
            return;
        };
        let content = result.map(|bytes| Content { bytes, uris: None });
        deliver(id, window_id, serial, Received { type_, content });
    });
    tracing::trace!("loading {name} for drop {}", id.into_raw());
}

/// Loads a data representation, bounded by [`MAX_DATA_BYTES`].
fn load_data(
    provider: &NSItemProvider,
    uti: &NSString,
    done: impl Fn(Result<Vec<u8>, DndError>) + Send + 'static,
) {
    let name = uti.to_string();
    let block = RcBlock::new(move |data: *mut NSData, error: *mut NSError| {
        // SAFETY: the item provider passes a valid object or null for both arguments.
        let (data, error) = unsafe { (data.as_ref(), error.as_ref()) };
        done(data_result(&name, data, error));
    });
    // SAFETY: the block captures only `Send` values and may run on any thread.
    let _progress: Retained<NSProgress> =
        unsafe { provider.loadDataRepresentationForTypeIdentifier_completionHandler(uti, &block) };
}

fn data_result(uti: &str, data: Option<&NSData>, error: Option<&NSError>) -> Result<Vec<u8>, DndError> {
    let Some(data) = data else {
        let reason = error.map_or_else(|| "no data".to_owned(), |error| error.localizedDescription().to_string());
        return Err(DndError::Load { uti: uti.to_owned(), reason });
    };

    let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
    check_size(uti, len, u64::try_from(MAX_DATA_BYTES).unwrap_or(u64::MAX))?;

    Ok(data.to_vec())
}

/// Collects one URI per item and delivers the list when every item answered.
struct UriCollector {
    remaining: usize,
    uris: Vec<Option<Result<String, DndError>>>,
    target: Option<(DataTransferId, WindowId, AsyncRequestSerial)>,
}

impl UriCollector {
    fn put(&mut self, index: usize, uri: Result<String, DndError>) {
        if let Some(slot) = self.uris.get_mut(index) {
            if slot.is_none() {
                *slot = Some(uri);
                self.remaining = self.remaining.saturating_sub(1);
            }
        }

        if self.remaining > 0 {
            return;
        }

        let Some((id, window_id, serial)) = self.target.take() else {
            return;
        };

        let mut uris = Vec::new();
        let mut first_error = None;
        for uri in self.uris.drain(..).flatten() {
            match uri {
                Ok(uri) => uris.push(uri),
                Err(error) => {
                    first_error.get_or_insert(error);
                },
            }
        }

        let content = match first_error {
            Some(error) if uris.is_empty() => Err(error),
            _ => Ok(Content { bytes: uri_list_bytes(&uris), uris: Some(uris) }),
        };
        deliver(id, window_id, serial, Received {
            type_: ReceivedType::Hint(TypeHint::UriList),
            content,
        });
    }
}

/// Loads a URI for every item: a URL representation when there is one, else a copy of the file
/// representation of its first type.
fn fetch_uris(
    providers: &[Retained<NSItemProvider>],
    id: DataTransferId,
    window_id: WindowId,
    serial: AsyncRequestSerial,
) {
    let collector = Arc::new(Mutex::new(UriCollector {
        remaining: providers.len(),
        uris: (0..providers.len()).map(|_| None).collect(),
        target: Some((id, window_id, serial)),
    }));
    let put = move |collector: &Arc<Mutex<UriCollector>>, index, uri| {
        if let Ok(mut collector) = collector.lock() {
            collector.put(index, uri);
        } else {
            tracing::error!("the URI collector of drop {} is poisoned", id.into_raw());
        }
    };

    for (index, provider) in providers.iter().enumerate() {
        let types = provider.registeredTypeIdentifiers();
        let url_type = types.iter().find(|uti| URL_TYPES.contains(&&*uti.to_string()));
        let collector = Arc::clone(&collector);
        if let Some(uti) = url_type {
            let name = uti.to_string();
            load_data(provider, &uti, move |bytes| {
                put(&collector, index, bytes.and_then(|bytes| uri_from_data(&name, &bytes)));
            });
        } else if let Some(uti) = types.iter().next() {
            let directory = copy_directory(id, serial, index);
            load_file(provider, &uti, directory, move |uri| put(&collector, index, uri));
        } else {
            put(&collector, index, Err(DndError::NoSuchType("UriList".to_owned())));
        }
    }
}

/// The directory a dropped file is copied into.
fn copy_directory(id: DataTransferId, serial: AsyncRequestSerial, index: usize) -> PathBuf {
    std::env::temp_dir()
        .join("winit-dnd")
        .join(format!("{}-{serial:?}-{index}", id.into_raw()).replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', ""))
}

/// Loads a file representation and copies it into `directory`, bounded by [`MAX_FILE_BYTES`].
fn load_file(
    provider: &NSItemProvider,
    uti: &NSString,
    directory: PathBuf,
    done: impl Fn(Result<String, DndError>) + Send + 'static,
) {
    let name = uti.to_string();
    let block = RcBlock::new(move |url: *mut NSURL, error: *mut NSError| {
        // SAFETY: the item provider passes a valid object or null for both arguments.
        let (url, error) = unsafe { (url.as_ref(), error.as_ref()) };
        done(copy_file(&name, url, error, &directory));
    });
    // SAFETY: the block captures only `Send` values and may run on any thread.
    let _progress: Retained<NSProgress> =
        unsafe { provider.loadFileRepresentationForTypeIdentifier_completionHandler(uti, &block) };
}

fn copy_file(
    uti: &str,
    url: Option<&NSURL>,
    error: Option<&NSError>,
    directory: &Path,
) -> Result<String, DndError> {
    let Some(source) = url.and_then(NSURL::path) else {
        let reason = error.map_or_else(|| "no file".to_owned(), |error| error.localizedDescription().to_string());
        return Err(DndError::Load { uti: uti.to_owned(), reason });
    };

    let source = PathBuf::from(source.to_string());
    let failed = |reason: io::Error| DndError::Copy { path: source.clone(), reason: reason.to_string() };
    let len = fs::metadata(&source).map_err(failed)?.len();
    check_size(uti, len, MAX_FILE_BYTES)?;

    let name = copy_name(&source).ok_or_else(|| DndError::NotAUri(uti.to_owned()))?;
    fs::create_dir_all(directory).map_err(failed)?;
    let target = directory.join(name);
    fs::copy(&source, &target).map_err(failed)?;

    let target_name = NSString::from_str(&target.to_string_lossy());
    NSURL::fileURLWithPath(&target_name)
        .absoluteString()
        .map(|uri| uri.to_string())
        .ok_or_else(|| DndError::NotAUri(uti.to_owned()))
}

/// Delivers received data on the main thread.
fn deliver(id: DataTransferId, window_id: WindowId, serial: AsyncRequestSerial, value: Received) {
    DispatchQueue::main().exec_async(move || {
        emit(window_id, WindowEvent::DataTransferReceived { id, serial, value: Arc::new(value) });
    });
}

/// The items of an armed drag of `window_id`, which turns active. Empty when nothing is armed.
pub(crate) fn lift(mtm: MainThreadMarker, window_id: WindowId) -> Retained<NSArray<UIDragItem>> {
    let state = state(mtm);
    let mut outgoing = state.outgoing.borrow_mut();
    let armed_here =
        matches!(*outgoing, Some(Outgoing::Armed { window_id: armed, .. }) if armed == window_id);
    if !armed_here {
        return NSArray::new();
    }

    let Some(Outgoing::Armed { id, window_id, data, actions, icon }) = outgoing.take() else {
        return NSArray::new();
    };

    let items = drag_items(mtm, data);
    if items.is_empty() {
        drop(outgoing);
        emit(window_id, WindowEvent::OutgoingDragCanceled { id });
        return NSArray::new();
    }

    *outgoing = Some(Outgoing::Active { id, window_id, actions, icon });
    NSArray::from_retained_slice(&items)
}

/// Whether the active outgoing drag offers move.
pub(crate) fn allows_move(mtm: MainThreadMarker) -> bool {
    match &*state(mtm).outgoing.borrow() {
        Some(Outgoing::Active { actions, .. } | Outgoing::Armed { actions, .. }) => {
            offers_move(actions)
        },
        None => false,
    }
}

/// The outgoing drag ended with `operation`.
pub(crate) fn drag_ended(mtm: MainThreadMarker, operation: UIDropOperation) {
    let ended = state(mtm).outgoing.borrow_mut().take();
    let Some(Outgoing::Active { id, window_id, .. }) = ended else {
        return;
    };

    let event = match action_for_operation(operation) {
        Some(action) => WindowEvent::OutgoingDragDropped { id, action: Some(action) },
        None => WindowEvent::OutgoingDragCanceled { id },
    };
    emit(window_id, event);
}

/// The lift preview of the active drag: its icon at the icon offset, or nothing visible.
pub(crate) fn lift_preview(
    mtm: MainThreadMarker,
    view: &UIView,
    location: CGPoint,
) -> Option<Retained<UITargetedDragPreview>> {
    let state = state(mtm);
    let outgoing = state.outgoing.borrow();
    let Some(Outgoing::Active { icon, .. }) = &*outgoing else {
        return None;
    };

    let scale = view.contentScaleFactor();
    let (preview, offset): (Retained<UIView>, (i32, i32)) =
        if let Some(shown) = icon.as_ref().and_then(|icon| icon_view(mtm, icon, scale)) {
            shown
        } else {
            let frame = CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize { width: 1.0, height: 1.0 },
            };
            (UIView::initWithFrame(mtm.alloc(), frame), (0, 0))
        };

    let size = preview.frame().size;
    let center = preview_center(location, offset, size, scale);
    let target = UIDragPreviewTarget::initWithContainer_center(mtm.alloc(), view, center);
    let parameters = UIDragPreviewParameters::init(mtm.alloc());
    Some(UITargetedDragPreview::initWithView_parameters_target(
        mtm.alloc(),
        &preview,
        &parameters,
        &target,
    ))
}

/// An image view of an RGBA icon, one icon pixel per physical pixel.
fn icon_view(mtm: MainThreadMarker, icon: &DragIcon, scale: f64) -> Option<(Retained<UIView>, (i32, i32))> {
    let Some(rgba) = icon.icon.cast_ref::<RgbaIcon>() else {
        tracing::warn!("only RGBA drag icons are shown on iOS");
        return None;
    };

    let width = usize::try_from(rgba.width()).ok()?;
    let height = usize::try_from(rgba.height()).ok()?;
    let row = width.checked_mul(4)?;
    let data = CFData::from_bytes(rgba.buffer());
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let space = CGColorSpace::new_device_rgb()?;
    // SAFETY: the provider holds `height` rows of `width` RGBA pixels, as `RgbaIcon` checks.
    let image = unsafe {
        CGImage::new(
            width,
            height,
            8,
            32,
            row,
            Some(&space),
            CGBitmapInfo(CGImageAlphaInfo::Last.0),
            Some(&provider),
            ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;
    let image = UIImage::initWithCGImage_scale_orientation(
        UIImage::alloc(),
        &image,
        scale,
        UIImageOrientation::Up,
    );
    let view = UIImageView::initWithImage(mtm.alloc(), Some(&image));

    Some((Retained::into_super(view), (icon.offset_x, icon.offset_y)))
}

/// One registration of a type identifier on an item provider.
struct Registration {
    uti: String,
    query: UtiType,
}

/// Builds the drag items of outgoing data: the first item carries every type and the first URI,
/// every further URI gets an item of its own.
fn drag_items(mtm: MainThreadMarker, data: Box<dyn DataTransferSend>) -> Vec<Retained<UIDragItem>> {
    let mut registrations = Vec::new();
    let mut has_uris = false;
    data.for_each_available_type(&mut |type_| {
        if let Some(uti) = type_.cast_ref::<UtiType>() {
            if !URL_TYPES.contains(&&*uti.identifier) {
                registrations.push(Registration { uti: uti.identifier.to_string(), query: uti.clone() });
            }
        } else if let Some(hint) = type_.hint() {
            if hint == TypeHint::UriList {
                has_uris = true;
            } else {
                for uti in utis_for_hint(hint) {
                    if !registrations.iter().any(|known: &Registration| known.uti == uti) {
                        registrations.push(Registration { uti: uti.to_owned(), query: UtiType::new(uti) });
                    }
                }
            }
        }

        ControlFlow::Continue(())
    });

    let uris = if has_uris {
        match data.data_for_type(&TypeHint::UriList) {
            Some(SendData::Uris(uris)) => uris,
            Some(SendData::String(uri)) => vec![uri],
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let shared = Arc::new(Mutex::new(data));
    let mut uris = uris.into_iter().take(MAX_ITEMS);
    let first = NSItemProvider::new();
    for registration in registrations {
        register_send(&first, registration, Arc::clone(&shared));
    }

    if let Some(uri) = uris.next() {
        register_uri(&first, &uri);
    }

    let mut items = Vec::new();
    if !first.registeredTypeIdentifiers().is_empty() {
        items.push(UIDragItem::initWithItemProvider(mtm.alloc(), &first));
    }

    for uri in uris {
        let provider = NSItemProvider::new();
        register_uri(&provider, &uri);
        items.push(UIDragItem::initWithItemProvider(mtm.alloc(), &provider));
    }

    items
}

/// Registers `uti` with data the application supplies when another application loads it.
fn register_send(provider: &NSItemProvider, registration: Registration, data: Arc<Mutex<Box<dyn DataTransferSend>>>) {
    let Registration { uti, query } = registration;
    let block = RcBlock::new(
        move |completion: NonNull<DynBlock<dyn Fn(*mut NSData, *mut NSError)>>| -> *mut NSProgress {
            let bytes = match data.lock() {
                Ok(data) => data.data_for_type(&query).and_then(send_bytes),
                Err(_poisoned) => {
                    tracing::warn!("the data of the drag is poisoned; sending nothing");
                    None
                },
            };
            complete(completion, bytes);
            ptr::null_mut()
        },
    );
    // SAFETY: the block captures only `Send` and `Sync` values and may run on any thread.
    unsafe {
        provider.registerDataRepresentationForTypeIdentifier_visibility_loadHandler(
            &NSString::from_str(&uti),
            NSItemProviderRepresentationVisibility::All,
            &block,
        );
    }
}

/// Registers a URI as `public.url`, and as `public.file-url` for a file URI.
fn register_uri(provider: &NSItemProvider, uri: &str) {
    for uti in URL_TYPES {
        if *uti == "public.file-url" && !uri.starts_with("file:") {
            continue;
        }

        let bytes = uri.as_bytes().to_vec();
        let block = RcBlock::new(
            move |completion: NonNull<DynBlock<dyn Fn(*mut NSData, *mut NSError)>>| -> *mut NSProgress {
                complete(completion, Some(bytes.clone()));
                ptr::null_mut()
            },
        );
        // SAFETY: the block captures only `Send` and `Sync` values and may run on any thread.
        unsafe {
            provider.registerDataRepresentationForTypeIdentifier_visibility_loadHandler(
                &NSString::from_str(uti),
                NSItemProviderRepresentationVisibility::All,
                &block,
            );
        }
    }
}

/// The bytes of outgoing data.
fn send_bytes(data: SendData) -> Option<Vec<u8>> {
    match data {
        SendData::String(text) => Some(text.into_bytes()),
        SendData::Bytes(bytes) => Some(bytes),
        SendData::Uris(uris) => Some(uri_list_bytes(&uris)),
        _ => None,
    }
}

/// Answers a load request of another application.
fn complete(completion: NonNull<DynBlock<dyn Fn(*mut NSData, *mut NSError)>>, bytes: Option<Vec<u8>>) {
    // SAFETY: the item provider passes a valid completion block for the duration of the call.
    let completion = unsafe { completion.as_ref() };
    if let Some(bytes) = bytes {
        let data = NSData::from_vec(bytes);
        completion.call((Retained::as_ptr(&data).cast_mut(), ptr::null_mut()));
    } else {
        // SAFETY: the domain is a constant string of Foundation.
        let error = NSError::new(ITEM_UNAVAILABLE, unsafe { NSItemProviderErrorDomain });
        completion.call((ptr::null_mut(), Retained::as_ptr(&error).cast_mut()));
    }
}


#[cfg(test)]
mod tests {
    use std::path::Path;

    use objc2_core_foundation::{CGPoint, CGSize};
    use objc2_ui_kit::UIDropOperation;
    use winit_core::data_transfer::{TransferType, TypeHint};
    use winit_core::event_loop::DndAction;

    use super::*;

    #[test]
    fn known_types_map_to_hints() {
        assert_eq!(hint_for_uti("public.utf8-plain-text"), Some(TypeHint::Plaintext));
        assert_eq!(hint_for_uti("public.plain-text"), Some(TypeHint::Plaintext));
        assert_eq!(hint_for_uti("public.html"), Some(TypeHint::Html));
        assert_eq!(hint_for_uti("public.url"), Some(TypeHint::UriList));
        assert_eq!(hint_for_uti("public.file-url"), Some(TypeHint::UriList));
        assert_eq!(hint_for_uti("public.png"), Some(TypeHint::Image { extension_hint: Some("png") }));
        assert_eq!(hint_for_uti("public.image"), Some(TypeHint::Image { extension_hint: None }));
        assert_eq!(hint_for_uti("public.audio"), Some(TypeHint::Audio { extension_hint: None }));
        assert_eq!(hint_for_uti("com.example.private"), None);
        assert_eq!(hint_for_uti(""), None);
    }

    #[test]
    fn hints_register_concrete_types_only() {
        assert_eq!(utis_for_hint(TypeHint::Plaintext), ["public.utf8-plain-text", "public.plain-text"]);
        let images = utis_for_hint(TypeHint::Image { extension_hint: None });
        assert!(images.contains(&"public.png"));
        assert!(images.contains(&"public.jpeg"));
        assert!(!images.contains(&"public.image"));
        assert_eq!(utis_for_hint(TypeHint::Image { extension_hint: Some("png") }), ["public.png"]);
        assert!(utis_for_hint(TypeHint::UriList).is_empty());
    }

    #[test]
    fn uti_types_match_by_identifier_or_hint() {
        let png = UtiType::new("public.png");
        assert!(png.matches(&UtiType::new("public.png")));
        assert!(!png.matches(&UtiType::new("public.jpeg")));
        assert!(png.matches(&TypeHint::Image { extension_hint: None }));
        assert!(!png.matches(&TypeHint::Plaintext));
        assert!(!UtiType::new("com.example.private").matches(&TypeHint::Plaintext));
    }

    #[test]
    fn copy_is_always_allowed_and_move_only_locally() {
        let move_copy = [DndAction::Move, DndAction::Copy];
        assert_eq!(drop_action(&move_copy, true), Some(DndAction::Move));
        assert_eq!(drop_action(&move_copy, false), Some(DndAction::Copy));
        assert_eq!(drop_action(&[DndAction::Move], false), None);
        assert_eq!(drop_action(&[DndAction::Link, DndAction::Ask], true), None);
        assert_eq!(drop_action(&[], true), None);
    }

    #[test]
    fn operations_follow_actions() {
        assert_eq!(operation_for(Some(DndAction::Move), &[DndAction::Move]), UIDropOperation::Move);
        assert_eq!(operation_for(Some(DndAction::Copy), &[DndAction::Copy]), UIDropOperation::Copy);
        assert_eq!(operation_for(None, &[]), UIDropOperation::Cancel);
        assert_eq!(operation_for(None, &[DndAction::Link]), UIDropOperation::Forbidden);
        assert_eq!(action_for_operation(UIDropOperation::Copy), Some(DndAction::Copy));
        assert_eq!(action_for_operation(UIDropOperation::Move), Some(DndAction::Move));
        assert_eq!(action_for_operation(UIDropOperation::Cancel), None);
        assert_eq!(action_for_operation(UIDropOperation::Forbidden), None);
        assert!(offers_move(&[DndAction::Copy, DndAction::Move]));
        assert!(!offers_move(&[DndAction::Copy]));
    }

    #[test]
    fn url_data_holds_one_uri() {
        assert_eq!(uri_from_data("public.url", b"https://example.org/\0").unwrap(), "https://example.org/");
        assert_eq!(uri_from_data("public.url", b" file:///a%20b \n").unwrap(), "file:///a%20b");
        assert_eq!(uri_from_data("public.url", b""), Err(DndError::NotAUri("public.url".to_owned())));
        assert_eq!(uri_from_data("public.url", b"a\r\nb"), Err(DndError::NotAUri("public.url".to_owned())));
        assert_eq!(uri_from_data("public.url", &[0xff, 0xfe]), Err(DndError::NotUtf8("public.url".to_owned())));
    }

    #[test]
    fn uri_lists_end_lines_with_crlf() {
        assert_eq!(uri_list_bytes(&[]), b"");
        assert_eq!(uri_list_bytes(&["a:b".to_owned(), "c:d".to_owned()]), b"a:b\r\nc:d\r\n");
    }

    #[test]
    fn sizes_are_bounded() {
        assert!(check_size("public.png", 0, 10).is_ok());
        assert!(check_size("public.png", 10, 10).is_ok());
        assert_eq!(
            check_size("public.png", 11, 10),
            Err(DndError::TooLarge { uti: "public.png".to_owned(), len: 11, limit: 10 })
        );
        assert!(check_size("public.png", u64::MAX, MAX_FILE_BYTES).is_err());
    }

    #[test]
    fn copies_keep_the_file_name_only() {
        assert_eq!(copy_name(Path::new("/a/b/photo.jpg")).unwrap(), "photo.jpg");
        assert_eq!(copy_name(Path::new("/")), None);
        assert_eq!(copy_name(Path::new("/a/..")), None);
    }

    #[test]
    fn preview_sits_at_the_offset() {
        let center = preview_center(
            CGPoint { x: 10.0, y: 20.0 },
            (-8, 4),
            CGSize { width: 16.0, height: 8.0 },
            2.0,
        );
        assert!((center.x - 14.0).abs() < 1e-9);
        assert!((center.y - 26.0).abs() < 1e-9);
    }

    #[test]
    fn errors_name_their_input() {
        let error = DndError::TooLarge { uti: "public.png".to_owned(), len: 3, limit: 2 };
        assert_eq!(error.to_string(), "public.png holds 3 bytes, more than the limit of 2");
        assert!(DndError::NoSuchType("public.html".to_owned()).to_string().contains("public.html"));
    }
}
