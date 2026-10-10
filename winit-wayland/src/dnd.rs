//! Types related to drag-and-drop and data transfer on Wayland.

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, BufRead, Cursor, ErrorKind, Write};
use std::ops::{BitOr, Deref};
use std::sync::Arc;
use std::time::Duration;

use calloop::PostAction;
use calloop::timer::{TimeoutAction, Timer};
use dpi::{LogicalPosition, PhysicalPosition};
use sctk::data_device_manager::WritePipe;
use sctk::data_device_manager::data_device::{DataDeviceData, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::{DataSourceHandler, DragSource as SctkDragSource};
use sctk::reexports::client::backend::ObjectId;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction as WlDndAction;
use wayland_client::protocol::wl_data_offer::WlDataOffer;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy, QueueHandle};
use winit_core::data_transfer::{
    DataTransfer, DataTransferId, DataTransferSend, SendData, TransferType, TypeHint, TypedData,
};
use winit_core::event::WindowEvent;
use winit_core::event_loop::DndAction;
use winit_core::window::WindowId;

use crate::make_data_transfer_id;
use crate::state::WinitState;

fn encode_uri_list<I>(uri_list: I) -> Vec<u8>
where
    I: IntoIterator,
    I::Item: AsRef<OsStr>,
{
    let mut out = Vec::new();

    for uri in uri_list {
        out.extend_from_slice(OsStr::new(&uri).as_encoded_bytes());
        out.extend_from_slice(b"\r\n");
    }

    out
}

impl DataSourceHandler for WinitState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
        // This method isn't a necessary part of the protocol, it's a holdover from the first
        // version of DnD in Wayland and now just serves as a hint.
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        let Some(data) = self.dnd_state.send_drag_data_mut() else {
            // TODO: Is there a way to explicitly express that the data was not sent?
            return;
        };

        let mime = MimeType::parse(mime);

        let Some(send_data) = data.data_for_type(&mime) else {
            return;
        };

        let mut encoder = match send_data {
            SendData::Uris(strings) => Cursor::new(encode_uri_list(strings)),
            SendData::String(str) => match mime.parse_charset() {
                Ok(Charset::Utf8) => Cursor::new(str.into_bytes()),
                Err(e) => {
                    tracing::error!("{e}");
                    return;
                },
            },
            SendData::Bytes(binary) => Cursor::new(binary),
            _ => return,
        };

        let _ = self.loop_handle.insert_source(fd, move |_, file, _| {
            // Safety: We only mutate `file` in-place and do not replace and drop it.
            let file = unsafe { file.get_mut() };
            loop {
                let Ok(encoded_bytes) = encoder.fill_buf() else {
                    return PostAction::Remove;
                };

                match file.write(encoded_bytes) {
                    Ok(0) => {
                        break PostAction::Remove;
                    },
                    Ok(consumed) => {
                        encoder.consume(consumed);
                    },
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        break PostAction::Continue;
                    },
                    Err(_) => {
                        break PostAction::Remove;
                    },
                }
            }
        });
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {
        let Some(current_drag) = self.dnd_state.send_drag() else {
            return;
        };

        let window_id = current_drag.window_id;
        let id = current_drag.data_transfer_id;

        self.events_sink.push_window_event(WindowEvent::OutgoingDragCanceled { id }, window_id);
        self.dnd_state.clear_send_drag();
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {
        // The outcome is reported on `dnd_finished` or `cancelled`.
    }

    fn dnd_finished(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wayland_client::protocol::wl_data_source::WlDataSource,
    ) {
        if let Some(current_drag) = self.dnd_state.send_drag() {
            self.events_sink.push_window_event(
                WindowEvent::OutgoingDragDropped {
                    id: current_drag.data_transfer_id,
                    action: dnd_action_wl_to_winit(current_drag.selected_action),
                },
                current_drag.window_id,
            );
        }

        self.dnd_state.clear_send_drag();
    }

    fn action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        action: WlDndAction,
    ) {
        self.dnd_state.set_target_drag_action(action);
    }
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Hash)]
enum Charset {
    #[default]
    Utf8,
}

/// MIME type as string, with an optional hint detected from the MIME type.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct MimeType {
    mime: Arc<str>,
    hint: Option<TypeHint>,
}

// MIME types
// Files
const TEXT_URI_LIST: &str = "text/uri-list";
// Plaintext
const TEXT_PLAIN: &str = "text/plain";
const TEXT_PLAIN_CHARSET_UTF8: &str = "text/plain; charset=utf-8";
// HTML
const TEXT_HTML: &str = "text/html";
const TEXT_HTML_CHARSET_UTF8: &str = "text/html; charset=utf-8";
// RTF
const APPLICATION_RTF: &str = "application/rtf";
// Audio
const AUDIO_AAC: &str = "audio/aac";
const AUDIO_AIFF: &str = "audio/aiff";
const AUDIO_FLAC: &str = "audio/flac";
const AUDIO_WAV: &str = "audio/wav";
const AUDIO_WAVE: &str = "audio/wave";
const AUDIO_X_WAV: &str = "audio/x-wav";
const AUDIO_VND_WAV: &str = "audio/vnd.wav";
const AUDIO_VND_WAVE: &str = "audio/vnd.wave";
const AUDIO_MPEG: &str = "audio/mpeg";
const AUDIO_OGG: &str = "audio/ogg";
// Image
const IMAGE_BMP: &str = "image/bmp";
const IMAGE_GIF: &str = "image/gif";
const IMAGE_JPEG: &str = "image/jpeg";
const IMAGE_PJPEG: &str = "image/pjpeg";
const IMAGE_PNG: &str = "image/png";
const IMAGE_SVG: &str = "image/svg+xml";
const IMAGE_TIFF: &str = "image/tiff";
const IMAGE_WEBP: &str = "image/webp";
const IMAGE_X_ICON: &str = "image/x-icon";
const IMAGE_RAW: &str = "image/x-panasonic-raw";

#[derive(Debug)]
struct UnexpectedCharsetError<'a>(&'a str);

impl std::error::Error for UnexpectedCharsetError<'_> {}

impl fmt::Display for UnexpectedCharsetError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unsupported charset: {}", self.0)
    }
}

impl MimeType {
    const MIME_HINT_MAP: &[(&str, TypeHint)] = &[
        // Files
        (TEXT_URI_LIST, TypeHint::UriList),
        // Plaintext
        (TEXT_PLAIN, TypeHint::Plaintext),
        (TEXT_PLAIN_CHARSET_UTF8, TypeHint::Plaintext),
        // HTML
        (TEXT_HTML, TypeHint::Html),
        (TEXT_HTML_CHARSET_UTF8, TypeHint::Html),
        // RTF
        (APPLICATION_RTF, TypeHint::Rtf),
        // Audio
        (AUDIO_AAC, TypeHint::Audio { extension_hint: Some("aac") }),
        (AUDIO_AIFF, TypeHint::Audio { extension_hint: Some("aif") }),
        (AUDIO_FLAC, TypeHint::Audio { extension_hint: Some("flac") }),
        (AUDIO_VND_WAV, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_VND_WAVE, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_WAV, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_WAVE, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_X_WAV, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_OGG, TypeHint::Audio { extension_hint: Some("ogg") }),
        (AUDIO_MPEG, TypeHint::Audio { extension_hint: Some("mp3") }),
        // Image
        (IMAGE_BMP, TypeHint::Image { extension_hint: Some("bmp") }),
        (IMAGE_GIF, TypeHint::Image { extension_hint: Some("gif") }),
        (IMAGE_JPEG, TypeHint::Image { extension_hint: Some("jpg") }),
        (IMAGE_PJPEG, TypeHint::Image { extension_hint: Some("jpg") }),
        (IMAGE_PNG, TypeHint::Image { extension_hint: Some("png") }),
        (IMAGE_RAW, TypeHint::Image { extension_hint: Some("raw") }),
        (IMAGE_SVG, TypeHint::Image { extension_hint: Some("svg") }),
        (IMAGE_TIFF, TypeHint::Image { extension_hint: Some("tiff") }),
        (IMAGE_WEBP, TypeHint::Image { extension_hint: Some("webp") }),
        (IMAGE_X_ICON, TypeHint::Image { extension_hint: Some("ico") }),
    ];

    // Returns an iterator so that things like the multiple charsets for plaintext/HTML
    // and the multiple ways of expressing .wav work correctly.
    pub(crate) fn from_dyn(type_: &dyn TransferType) -> impl Iterator<Item = Self> {
        let downcast = type_.cast_ref::<Self>().cloned();
        let downcast_failed = downcast.is_none();
        // This filter is a bit hacky, but it's the only way to ensure that we always
        // return the same type.
        let from_hint = downcast_failed
            .then_some(
                Self::MIME_HINT_MAP
                    .iter()
                    .filter(move |(_, haystack)| TransferType::matches(haystack, type_))
                    .map(move |(mime, _)| Self {
                        mime: mime.to_string().into(),
                        hint: type_.hint(),
                    }),
            )
            .into_iter()
            .flatten();

        downcast.into_iter().chain(from_hint)
    }

    // TODO: We should properly parse MIME types using `mime` or a similar crate.
    fn parse_charset(&self) -> Result<Charset, UnexpectedCharsetError<'_>> {
        let Some((_, charset)) = self
            .mime
            .split_once(';')
            .and_then(|(_essence, options)| options.split_once("charset="))
        else {
            return Ok(Default::default());
        };

        let charset = charset.split_once(',').map(|(first, _)| first).unwrap_or(charset).trim();

        if charset == "utf-8" { Ok(Charset::Utf8) } else { Err(UnexpectedCharsetError(charset)) }
    }

    fn parse(mime: String) -> Self {
        let hint = Self::MIME_HINT_MAP
            .iter()
            .find_map(|(haystack, hint)| (*haystack == &*mime).then_some(*hint))
            .or_else(|| {
                if mime.starts_with("image/") {
                    Some(TypeHint::Image { extension_hint: None })
                } else if mime.starts_with("audio/") {
                    Some(TypeHint::Audio { extension_hint: None })
                } else {
                    None
                }
            });

        Self { mime: mime.into(), hint }
    }
}

impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.mime.fmt(f)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct UnknownTypeHint(pub TypeHint);

impl fmt::Display for UnknownTypeHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unknown type hint: {:?}", self.0)
    }
}

impl TryFrom<TypeHint> for MimeType {
    type Error = UnknownTypeHint;

    fn try_from(hint: TypeHint) -> Result<Self, Self::Error> {
        let mime = Self::MIME_HINT_MAP
            .iter()
            .find_map(|(mime, haystack)| (*haystack == hint).then_some(*mime))
            .ok_or(UnknownTypeHint(hint))?;

        Ok(Self { mime: mime.to_owned().into(), hint: Some(hint) })
    }
}

impl TransferType for MimeType {
    fn hint(&self) -> Option<TypeHint> {
        self.hint
    }

    fn matches(&self, other: &dyn TransferType) -> bool {
        if let Some(other_mime) = other.cast_ref::<Self>() {
            *self == *other_mime
        } else {
            // If either hint is `None`, return false
            self.hint().is_some_and(|hint| other.hint() == Some(hint))
        }
    }
}

type BytesResult = Result<Vec<u8>, Arc<io::Error>>;

/// Typed data transfer from another application.
#[derive(Debug)]
pub struct MimeData {
    mime_type: MimeType,
    result: BytesResult,
}

impl MimeData {
    pub(crate) fn new(mime_type: MimeType, result: BytesResult) -> Self {
        Self { mime_type, result }
    }

    fn data(&self) -> io::Result<&[u8]> {
        fn arc_to_io_error(arc: Arc<io::Error>) -> io::Error {
            io::Error::new(arc.kind(), arc)
        }

        self.result.as_deref().map_err(|e| arc_to_io_error(e.clone()))
    }
}

impl TypedData for MimeData {
    fn type_(&self) -> &dyn TransferType {
        &self.mime_type
    }

    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        let data = self.data().ok()?.to_owned();

        Some(Box::new(io::Cursor::new(data)))
    }

    fn try_as_bytes(&self) -> io::Result<Vec<u8>> {
        self.data().map(ToOwned::to_owned)
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        let data = self.data()?;

        Cursor::new(&data)
            .lines()
            .filter(|result| match result {
                Ok(s) => !s.starts_with('#'),
                // We want to maintain errors, so the final `collect` returns an error too
                Err(_) => true,
            })
            .collect()
    }

    fn try_as_string(&self) -> io::Result<String> {
        let charset = self.mime_type.parse_charset();

        let data = self.data()?;

        match charset {
            Ok(Charset::Utf8) => String::from_utf8(data.to_vec())
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }
}

/// A wrapper around `WlDataOffer`, implementing `DataTransfer`.
#[derive(Debug, Clone)]
pub struct DataOffer {
    mime_types: Arc<[MimeType]>,
    data: WlDataOffer,
    available_actions: WlDndAction,
    data_device_id: ObjectId,
    serial: u32,
    window_id: WindowId,
    /// Whether the offer was dropped.
    dropped: bool,
    /// The action of the dropped offer, final once it is not `ask`.
    dropped_action: WlDndAction,
    /// Transfers started and not yet read to the end.
    pending_transfers: usize,
    /// Whether `DragDropped` reached the application.
    drop_dispatched: bool,
}

/// How long a dropped offer may wait for its final action and transfers.
const DROP_TIMEOUT: Duration = Duration::from_secs(10);

/// What a dropped offer does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropEnd {
    Wait,
    Finish,
    Destroy,
}

/// Decides the end of a dropped offer from its action, open transfers and offer version.
pub(crate) fn drop_end(
    action: WlDndAction,
    pending_transfers: usize,
    drop_dispatched: bool,
    version: u32,
) -> DropEnd {
    if !drop_dispatched || pending_transfers > 0 || action == WlDndAction::Ask {
        DropEnd::Wait
    } else if version >= 3 && finishes_drop(action) {
        DropEnd::Finish
    } else {
        DropEnd::Destroy
    }
}

/// The final action for a drop that selected `ask`: the first copy or move in `actions` the
/// source offers.
pub(crate) fn ask_answer(actions: &[DndAction], source_actions: WlDndAction) -> Option<WlDndAction> {
    actions.iter().find_map(|action| {
        let wl = match action {
            DndAction::Copy => WlDndAction::Copy,
            DndAction::Move => WlDndAction::Move,
            _ => return None,
        };
        source_actions.contains(wl).then_some(wl)
    })
}

pub(crate) fn dnd_action_winit_to_wl(winit: DndAction) -> WlDndAction {
    match winit {
        DndAction::Move => WlDndAction::Move,
        DndAction::Copy => WlDndAction::Copy,
        DndAction::Ask => WlDndAction::Ask,
        _ => WlDndAction::empty(),
    }
}

pub(crate) fn dnd_action_wl_to_winit(wl: WlDndAction) -> Option<DndAction> {
    match wl {
        WlDndAction::Move => Some(DndAction::Move),
        WlDndAction::Copy => Some(DndAction::Copy),
        WlDndAction::Ask => Some(DndAction::Ask),
        _ => None,
    }
}

/// Whether a dropped offer with this selected action may be finished.
///
/// `wl_data_offer.finish` is only valid for a final copy or move. `ask` still awaits a last
/// `set_actions`, and without an action the drop was not accepted.
pub(crate) fn finishes_drop(selected_action: WlDndAction) -> bool {
    selected_action == WlDndAction::Copy || selected_action == WlDndAction::Move
}

impl DataOffer {
    pub(crate) fn transfer_id(&self) -> DataTransferId {
        make_data_transfer_id(self.data_device_id.clone(), self.serial)
    }

    pub(crate) fn first_mime_type(&self) -> Option<&MimeType> {
        self.mime_types.first()
    }

    pub(crate) fn serial(&self) -> u32 {
        self.serial
    }

    pub(crate) fn window_id(&self) -> WindowId {
        self.window_id
    }

    pub(crate) fn is_dropped(&self) -> bool {
        self.dropped
    }

    /// Sends the final action of a drop that selected `ask`. Other drops are left alone.
    pub(crate) fn answer_ask(&mut self, action_set: &[DndAction]) {
        if !self.dropped || self.dropped_action != WlDndAction::Ask {
            return;
        }

        match ask_answer(action_set, self.available_actions) {
            Some(action) => {
                self.data.set_actions(action, action);
                self.dropped_action = action;
            },
            None => self.dropped_action = WlDndAction::empty(),
        }
    }

    pub(crate) fn set_actions(&self, action_set: &[DndAction]) -> bool {
        let preferred_action = action_set.iter().find_map(|winit| {
            let wl = dnd_action_winit_to_wl(*winit);
            self.available_actions.intersects(wl).then_some(wl)
        });

        let any = preferred_action.is_some();

        let all_actions = action_set
            .iter()
            .copied()
            .map(dnd_action_winit_to_wl)
            .fold(WlDndAction::empty(), BitOr::bitor);

        self.data.set_actions(all_actions, preferred_action.unwrap_or(WlDndAction::empty()));

        any
    }

    pub(crate) fn find_type_dyn<'a>(&'a self, type_: &'a dyn TransferType) -> Option<&'a MimeType> {
        match type_.cast_ref::<MimeType>() {
            Some(mime_type) => Some(mime_type),
            None => {
                let hint = type_.hint()?;
                self.mime_types.iter().find(|mime_type| {
                    mime_type.hint().is_some_and(|haystack| haystack.matches(&hint))
                })
            },
        }
    }
}

impl Deref for DataOffer {
    type Target = WlDataOffer;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl DataTransfer for DataOffer {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> std::ops::ControlFlow<()>,
    ) {
        let _ = self.mime_types.iter().map(|mime| mime as &dyn TransferType).try_for_each(func);
    }
}

/// Wrapper for [`WlDataSource`], which exposes the types that are advertised by a data
/// transfer operation, along with the data that the source represents
#[derive(Debug)]
pub struct DragSource {
    pub(crate) data_transfer_id: DataTransferId,
    /// The `WlDataSource` generated from `data`.
    ///
    /// This is stored internally, as if this source is dropped then the
    /// drag operation will be cancelled.
    _data_source: SctkDragSource,
    /// The supplied [`DataTransferSend`].
    pub(crate) data: Box<dyn DataTransferSend>,
    pub(crate) selected_action: WlDndAction,
    pub(crate) window_id: WindowId,
    /// (Optionally) an icon for the drag-and-drop operation.
    _icon: Option<WlSurface>,
}

impl DragSource {
    pub(crate) fn new(
        data_transfer_id: DataTransferId,
        data_source: SctkDragSource,
        data: Box<dyn DataTransferSend>,
        icon: Option<WlSurface>,
        window_id: WindowId,
    ) -> Self {
        Self {
            data_transfer_id,
            _data_source: data_source,
            data,
            selected_action: WlDndAction::None,
            window_id,
            _icon: icon,
        }
    }

    /// Per-type data to be sent. See [`DataTransferSend`].
    pub fn data(&mut self) -> &mut dyn DataTransferSend {
        &mut *self.data
    }
}

/// The current state of an in-progress drag-and-drop operation.
#[derive(Debug, Default)]
pub struct DndState {
    receive_drag: Option<DataOffer>,
    send_drag: Option<DragSource>,
}

impl DndState {
    pub(crate) fn receive_drag(&self) -> Option<&DataOffer> {
        self.receive_drag.as_ref()
    }

    pub(crate) fn receive_drag_mut(&mut self) -> Option<&mut DataOffer> {
        self.receive_drag.as_mut()
    }

    /// Counts a transfer started on the offer `id`.
    pub(crate) fn transfer_started(&mut self, id: DataTransferId) {
        if let Some(offer) = self.receive_drag.as_mut().filter(|offer| offer.transfer_id() == id) {
            offer.pending_transfers = offer.pending_transfers.saturating_add(1);
        }
    }

    /// Counts a transfer on the offer `id` that was read to the end.
    pub(crate) fn transfer_done(&mut self, id: DataTransferId) {
        if let Some(offer) = self.receive_drag.as_mut().filter(|offer| offer.transfer_id() == id) {
            offer.pending_transfers = offer.pending_transfers.saturating_sub(1);
        }
    }

    /// Marks that the `DragDropped` of a dropped offer is being delivered.
    pub(crate) fn mark_drop_dispatched(&mut self) {
        if let Some(offer) = self.receive_drag.as_mut().filter(|offer| offer.dropped) {
            offer.drop_dispatched = true;
        }
    }

    /// Finishes or destroys a dropped offer once its action is final and its transfers ended.
    pub(crate) fn settle_drop(&mut self) {
        let Some(offer) = self.receive_drag.as_ref().filter(|offer| offer.dropped) else {
            return;
        };
        match drop_end(
            offer.dropped_action,
            offer.pending_transfers,
            offer.drop_dispatched,
            offer.version(),
        ) {
            DropEnd::Wait => {},
            DropEnd::Finish => {
                offer.finish();
                offer.destroy();
                self.receive_drag = None;
            },
            DropEnd::Destroy => {
                offer.destroy();
                self.receive_drag = None;
            },
        }
    }

    /// Destroys the dropped offer `id` that did not settle in time.
    pub(crate) fn expire_drop(&mut self, id: DataTransferId) {
        let expired = self
            .receive_drag
            .take_if(|offer| offer.dropped && offer.transfer_id() == id);
        if let Some(offer) = expired {
            tracing::warn!("A dropped offer did not get a final action in time; destroying it");
            offer.destroy();
        }
    }

    pub(crate) fn set_send_drag(&mut self, source: DragSource) {
        self.send_drag = Some(source);
    }

    pub(crate) fn send_drag(&self) -> Option<&DragSource> {
        self.send_drag.as_ref()
    }

    pub(crate) fn set_target_drag_action(&mut self, action: WlDndAction) {
        if let Some(source) = &mut self.send_drag {
            source.selected_action = action;
        }
    }

    /// Returns `true` if a drag operation was in progress, `false` if no drag operation was in
    /// progress.
    pub(crate) fn clear_send_drag(&mut self) -> bool {
        self.send_drag.take().is_some()
    }

    pub(crate) fn send_drag_data_mut(&mut self) -> Option<&mut dyn DataTransferSend> {
        self.send_drag.as_mut().map(|send| send.data())
    }
}

impl DataOfferHandler for WinitState {
    fn source_actions(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        offer: &mut DragOffer,
        actions: WlDndAction,
    ) {
        let _ = actions;
        let _ = offer;
        let _ = qh;
        let _ = conn;
        // Not implemented, but required for `DataDeviceHandler`.
    }

    fn selected_action(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        offer: &mut DragOffer,
        actions: WlDndAction,
    ) {
        let _ = actions;
        let _ = offer;
        let _ = qh;
        let _ = conn;
    }
}

impl DataDeviceHandler for WinitState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
        x: f64,
        y: f64,
        wl_surface: &WlSurface,
    ) {
        let Some(data) = data_device.data::<DataDeviceData>() else {
            return;
        };

        let Some(drag) = data.drag_offer() else {
            // Selections are not yet implemented
            return;
        };

        let window_id = crate::make_wid(wl_surface);

        let current_drag = drag.with_mime_types(|types| DataOffer {
            mime_types: types
                .iter()
                .map(|str| MimeType::parse(str.clone()))
                .collect::<Vec<_>>()
                .into(),
            available_actions: drag.source_actions,
            serial: drag.serial,
            data_device_id: data_device.id(),
            data: drag.inner().clone(),
            window_id,
            dropped: false,
            dropped_action: WlDndAction::empty(),
            pending_transfers: 0,
            drop_dispatched: false,
        });

        current_drag.set_actions(&[]);

        let id = current_drag.transfer_id();

        self.dnd_state.receive_drag = Some(current_drag);

        let scale_factor = self
            .windows
            .borrow()
            .get(&window_id)
            .map(|window| window.lock().unwrap().scale_factor())
            .unwrap_or(1.);
        let position: PhysicalPosition<f64> = LogicalPosition::new(x, y).to_physical(scale_factor);

        self.events_sink.push_window_event(
            WindowEvent::DragEntered { id, position: Some(position) },
            window_id,
        );
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, data_device: &WlDataDevice) {
        let Some(data) = data_device.data::<DataDeviceData>() else {
            return;
        };

        // A dropped offer stays until it is finished or destroyed in `settle_drop`.
        if self.dnd_state.receive_drag().is_some_and(DataOffer::is_dropped) {
            return;
        }

        if let Some(current_drag) = self.dnd_state.receive_drag() {
            self.events_sink.push_window_event(
                WindowEvent::DragLeft { id: current_drag.transfer_id() },
                current_drag.window_id(),
            );

            // An offer that was not dropped is only destroyed, never finished.
            self.dnd_state.receive_drag = None;
        }

        if let Some(drag) = data.drag_offer().filter(|drag| !drag.dropped) {
            drag.destroy();
        }
        if let Some(selection) = data.selection_offer() {
            selection.destroy();
        }
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        let Some(data) = data_device.data::<DataDeviceData>() else {
            return;
        };
        let Some(drag) = data.drag_offer() else {
            // Selections (copy/paste) are not yet implemented
            return;
        };

        // `selected_action` should only contain a single flag, but we check with `contains`
        // just in case we or the compositor misunderstood the spec.
        let proposed_action = if drag.selected_action.contains(WlDndAction::Move) {
            Some(DndAction::Move)
        } else if drag.selected_action.contains(WlDndAction::Copy) {
            Some(DndAction::Copy)
        } else if drag.selected_action.contains(WlDndAction::Ask) {
            Some(DndAction::Ask)
        } else {
            None
        };

        let Some(current_drag) = self.dnd_state.receive_drag() else {
            return;
        };

        let window_id = crate::make_wid(&drag.surface);

        let scale_factor = self
            .windows
            .borrow()
            .get(&window_id)
            .map(|window| window.lock().unwrap().scale_factor())
            .unwrap_or(1.);
        let position: PhysicalPosition<f64> = LogicalPosition::new(x, y).to_physical(scale_factor);

        self.events_sink.push_window_event(
            WindowEvent::DragPosition { id: current_drag.transfer_id(), position, proposed_action },
            window_id,
        );
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        // We don't handle selections right now.
    }

    fn drop_performed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
    ) {
        let Some(data) = data_device.data::<DataDeviceData>() else {
            return;
        };
        let Some(drag) = data.drag_offer() else {
            // Selections (copy/paste) are not yet implemented
            return;
        };

        let Some(current_drag) = self.dnd_state.receive_drag() else {
            return;
        };

        let window_id = crate::make_wid(&drag.surface);

        // `selected_action` should only contain a single flag, but we check with `contains`
        // just in case we or the compositor misunderstood the spec.
        let proposed_action = if drag.selected_action.contains(WlDndAction::Move) {
            Some(DndAction::Move)
        } else if drag.selected_action.contains(WlDndAction::Copy) {
            Some(DndAction::Copy)
        } else if drag.selected_action.contains(WlDndAction::Ask) {
            Some(DndAction::Ask)
        } else {
            None
        };

        let id = current_drag.transfer_id();
        self.events_sink.push_window_event(
            WindowEvent::DragDropped { id, proposed_action },
            window_id,
        );

        if let Some(offer) = self.dnd_state.receive_drag_mut() {
            offer.dropped = true;
            offer.dropped_action = drag.selected_action;
        }

        let timer = Timer::from_duration(DROP_TIMEOUT);
        let inserted = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.dispatched_events = true;
            state.dnd_state.expire_drop(id);
            TimeoutAction::Drop
        });
        if let Err(err) = inserted {
            tracing::warn!("Failed to arm the drop timeout: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use wayland_client::protocol::wl_data_device_manager::DndAction as WlDndAction;

    use winit_core::event_loop::DndAction;

    use super::{DropEnd, ask_answer, drop_end, finishes_drop};

    #[test]
    fn a_drop_waits_for_the_application_and_its_transfers() {
        assert_eq!(drop_end(WlDndAction::Copy, 0, false, 3), DropEnd::Wait);
        assert_eq!(drop_end(WlDndAction::Copy, 1, true, 3), DropEnd::Wait);
        assert_eq!(drop_end(WlDndAction::Move, 0, true, 3), DropEnd::Finish);
    }

    #[test]
    fn an_ask_drop_waits_for_the_answer() {
        assert_eq!(drop_end(WlDndAction::Ask, 0, true, 3), DropEnd::Wait);
    }

    #[test]
    fn a_drop_without_a_final_action_is_destroyed() {
        assert_eq!(drop_end(WlDndAction::empty(), 0, true, 3), DropEnd::Destroy);
        assert_eq!(drop_end(WlDndAction::Copy, 0, true, 2), DropEnd::Destroy);
    }

    #[test]
    fn ask_is_answered_with_an_offered_copy_or_move() {
        let both = WlDndAction::Copy | WlDndAction::Move | WlDndAction::Ask;
        assert_eq!(ask_answer(&[DndAction::Move], both), Some(WlDndAction::Move));
        assert_eq!(ask_answer(&[DndAction::Ask, DndAction::Copy], both), Some(WlDndAction::Copy));
        assert_eq!(ask_answer(&[DndAction::Move], WlDndAction::Copy | WlDndAction::Ask), None);
        assert_eq!(ask_answer(&[DndAction::Ask], both), None);
        assert_eq!(ask_answer(&[], both), None);
    }

    #[test]
    fn copy_and_move_finish() {
        assert!(finishes_drop(WlDndAction::Copy));
        assert!(finishes_drop(WlDndAction::Move));
    }

    #[test]
    fn other_actions_do_not_finish() {
        assert!(!finishes_drop(WlDndAction::None));
        assert!(!finishes_drop(WlDndAction::empty()));
        assert!(!finishes_drop(WlDndAction::Ask));
        assert!(!finishes_drop(WlDndAction::Copy | WlDndAction::Move));
    }
}
