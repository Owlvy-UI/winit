use std::collections::VecDeque;
use std::io;
use std::os::raw::*;
use std::str::Utf8Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use tracing::warn;
use winit_core::data_transfer::{DataTransfer, DataTransferId, TransferType, TypeHint, TypedData};
use winit_core::event_loop::{AsyncRequestSerial, DndAction};
use x11rb::protocol::xproto::{self, ConnectionExt};

use crate::atoms::*;
use crate::event_loop::{CookieResultExt, X11Error};
use crate::xdisplay::XConnection;
use crate::{XWindow, util};

/// The XDND action atoms that correspond to a [`DndAction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActionAtoms {
    copy: xproto::Atom,
    move_: xproto::Atom,
}

impl ActionAtoms {
    pub(crate) fn new(atoms: &Atoms) -> Self {
        Self { copy: atoms[XdndActionCopy], move_: atoms[XdndActionMove] }
    }

    /// The action named by an XDND action atom, if it is one winit supports.
    pub(crate) fn action(self, atom: xproto::Atom) -> Option<DndAction> {
        if atom == self.copy {
            Some(DndAction::Copy)
        } else if atom == self.move_ {
            Some(DndAction::Move)
        } else {
            None
        }
    }

    /// The XDND action atom for an action, if XDND has one that winit supports.
    pub(crate) fn atom(self, action: DndAction) -> Option<xproto::Atom> {
        match action {
            DndAction::Copy => Some(self.copy),
            DndAction::Move => Some(self.move_),
            _ => None,
        }
    }
}

/// Picks the action the target accepts.
///
/// `requested` is the action the source asked for in `XdndPosition`, `valid` the actions the
/// application accepts. The requested action is taken when the application accepts it.
/// Otherwise `Copy` is the fallback XDND allows a target to answer with.
pub(crate) fn choose_action(
    requested: Option<DndAction>,
    valid: &[DndAction],
) -> Option<DndAction> {
    let supported = |action: DndAction| matches!(action, DndAction::Copy | DndAction::Move);
    match requested {
        Some(action) if supported(action) && valid.contains(&action) => Some(action),
        _ => valid.contains(&DndAction::Copy).then_some(DndAction::Copy),
    }
}

/// How long the target waits for the answer to its `DELETE` request.
const DELETE_TIMEOUT: Duration = Duration::from_secs(2);

/// The accept flag and action atom of `XdndStatus` and `XdndFinished`, both 0 for a rejection.
fn accept_fields(action: Option<xproto::Atom>) -> (u32, xproto::Atom) {
    match action {
        Some(action) => (1, action),
        None => (0, x11rb::NONE),
    }
}

/// Whether a finished drop asks the source for `DELETE` before `XdndFinished`.
fn needs_delete(action: Option<DndAction>, received: bool) -> bool {
    action == Some(DndAction::Move) && received
}

/// A `DELETE` request awaiting its `SelectionNotify`, after which `XdndFinished` is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingDelete {
    this_window: xproto::Window,
    source_window: xproto::Window,
    deadline: Instant,
}

impl PendingDelete {
    fn new(this_window: xproto::Window, source_window: xproto::Window, now: Instant) -> Self {
        let deadline = now.checked_add(DELETE_TIMEOUT).unwrap_or(now);
        Self { this_window, source_window, deadline }
    }

    fn expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum UriListParseError {
    EmptyData,
    InvalidUtf8(#[allow(dead_code)] Utf8Error),
    HostnameSpecified(#[allow(dead_code)] String),
    UnexpectedProtocol(#[allow(dead_code)] String),
    UnresolvablePath(#[allow(dead_code)] io::Error),
    Io(#[allow(dead_code)] io::Error),
}

impl From<Utf8Error> for UriListParseError {
    fn from(e: Utf8Error) -> Self {
        UriListParseError::InvalidUtf8(e)
    }
}

impl From<io::Error> for UriListParseError {
    fn from(e: io::Error) -> Self {
        UriListParseError::UnresolvablePath(e)
    }
}

#[derive(Debug)]
pub struct SelectionReader {
    type_: SelectionType,
    data: Vec<u8>,
}

impl TypedData for SelectionReader {
    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        Some(Box::new(io::Cursor::new(self.data.clone())))
    }

    fn type_(&self) -> &dyn TransferType {
        &self.type_
    }

    fn try_as_string(&self) -> io::Result<String> {
        fn invalid_data<E>(err: E) -> io::Error
        where
            E: Into<Box<dyn std::error::Error + Send + Sync>>,
        {
            io::Error::new(io::ErrorKind::InvalidData, err)
        }

        fn decode_utf16_bytes(bytes: &[u8]) -> io::Result<String> {
            let utf16 = bytes
                .chunks_exact(2)
                .map(|chunk| {
                    let bytes: &[u8; 2] = chunk.try_into().unwrap();
                    u16::from_ne_bytes(*bytes)
                })
                .collect::<Vec<_>>();
            String::from_utf16(&utf16).map_err(invalid_data)
        }

        match self.type_.hint() {
            Some(TypeHint::Plaintext) | Some(TypeHint::Html) => std::str::from_utf8(&self.data)
                .map(|str| str.to_owned())
                .map_err(invalid_data)
                .or_else(|_| decode_utf16_bytes(&self.data)),
            Some(TypeHint::UriList) => String::from_utf8(self.data.clone()).map_err(invalid_data),
            _ => Err(io::ErrorKind::InvalidData.into()),
        }
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        if self.type_().hint() != Some(TypeHint::UriList) {
            return Err(io::ErrorKind::InvalidData.into());
        }

        Ok(self
            .try_as_string()?
            .split(['\n', '\r'])
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }
}

#[derive(Debug)]
pub struct DragState {
    // Populated by XdndEnter event handler
    pub version: c_long,
    pub transfer_id: DataTransferId,
    pub types: Arc<[SelectionType]>,
    // Populated by Xdnd* event handlers
    pub source_window: xproto::Window,
    // Populated by Xdnd* event handlers
    pub target_window: xproto::Window,
    // Populated by `fetch_data_transfer`
    pub pending_fetch_types: VecDeque<(AsyncRequestSerial, SelectionType)>,
    pub finished: Option<(XWindow, XWindow)>,
    /// The actions the application accepts, in order of preference.
    // Populated by `set_valid_dnd_actions`.
    pub valid_actions: Vec<DndAction>,
    /// The action the source requested in the last `XdndPosition`.
    pub requested_action: Option<DndAction>,
    /// Whether a data transfer of this drag completed.
    pub received: bool,
    /// The `DELETE` request sent after a move, if one is outstanding.
    pub delete: Option<PendingDelete>,
}

impl DragState {
    /// The action the target currently accepts, `None` when the drag is rejected.
    pub fn chosen_action(&self) -> Option<DndAction> {
        choose_action(self.requested_action, &self.valid_actions)
    }
}

/// A data transfer ID not handed out before, for incoming and outgoing drags alike.
pub(crate) fn next_transfer_id() -> DataTransferId {
    static DATA_TRANSFER_ID: AtomicI64 = AtomicI64::new(0);

    DataTransferId::from_raw(DATA_TRANSFER_ID.fetch_add(1, Ordering::Relaxed))
}

impl Default for DragState {
    fn default() -> Self {
        Self {
            version: Default::default(),
            transfer_id: next_transfer_id(),
            types: Default::default(),
            source_window: Default::default(),
            target_window: Default::default(),
            pending_fetch_types: Default::default(),
            finished: None,
            valid_actions: Vec::new(),
            requested_action: None,
            received: false,
            delete: None,
        }
    }
}

#[derive(Debug)]
pub struct Dnd {
    xconn: Arc<XConnection>,
    // If `None`, no drag operation is in progress.
    state: Option<DragState>,
}

#[derive(Debug)]
pub struct Selection {
    types: Arc<[SelectionType]>,
}

impl Selection {
    pub(crate) fn new(types: Arc<[SelectionType]>) -> Selection {
        Selection { types }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SelectionType {
    hint: Option<TypeHint>,
    atom: xproto::Atom,
}

impl SelectionType {
    pub(crate) fn new(atoms: &Atoms, atom: xproto::Atom) -> Self {
        let hint = hint_table(atoms)
            .iter()
            .find_map(|(haystack, hint)| (*haystack == atom).then_some(*hint));

        Self { hint, atom }
    }

    pub fn atom(&self) -> xproto::Atom {
        self.atom
    }

    /// The types offered for an outgoing type, in the order of the hint table.
    pub(crate) fn offered_for(atoms: &Atoms, type_: &dyn TransferType) -> Vec<Self> {
        if let Some(own) = type_.cast_ref::<Self>() {
            return vec![own.clone()];
        }

        hint_table(atoms)
            .into_iter()
            .filter(|(_, hint)| TransferType::matches(hint, type_))
            .map(|(atom, hint)| Self { hint: Some(hint), atom })
            .collect()
    }
}

/// The selection targets and the type hints they map to, preferred targets first.
fn hint_table(atoms: &Atoms) -> [(xproto::Atom, TypeHint); 28] {
    [
        // Files
        (atoms[TextUriList], TypeHint::UriList),
        // Plaintext
        (atoms[UTF8_STRING], TypeHint::Plaintext),
        (atoms[TextPlainCharsetUtf8], TypeHint::Plaintext),
        (atoms[TextPlain], TypeHint::Plaintext),
        (atoms[STRING], TypeHint::Plaintext),
        // HTML
        (atoms[TextHtmlCharsetUtf8], TypeHint::Html),
        (atoms[TextHtml], TypeHint::Html),
        // RTF
        (atoms[ApplicationRtf], TypeHint::Rtf),
        // Audio
        (atoms[AudioAac], TypeHint::Audio { extension_hint: Some("aac") }),
        (atoms[AudioAiff], TypeHint::Audio { extension_hint: Some("aif") }),
        (atoms[AudioFlac], TypeHint::Audio { extension_hint: Some("flac") }),
        (atoms[AudioVndWav], TypeHint::Audio { extension_hint: Some("wav") }),
        (atoms[AudioVndWave], TypeHint::Audio { extension_hint: Some("wav") }),
        (atoms[AudioWav], TypeHint::Audio { extension_hint: Some("wav") }),
        (atoms[AudioWave], TypeHint::Audio { extension_hint: Some("wav") }),
        (atoms[AudioXWav], TypeHint::Audio { extension_hint: Some("wav") }),
        (atoms[AudioOgg], TypeHint::Audio { extension_hint: Some("ogg") }),
        (atoms[AudioMpeg], TypeHint::Audio { extension_hint: Some("mp3") }),
        // Image
        (atoms[ImageBmp], TypeHint::Image { extension_hint: Some("bmp") }),
        (atoms[ImageGif], TypeHint::Image { extension_hint: Some("gif") }),
        (atoms[ImageJpeg], TypeHint::Image { extension_hint: Some("jpg") }),
        (atoms[ImagePjpeg], TypeHint::Image { extension_hint: Some("jpg") }),
        (atoms[ImagePng], TypeHint::Image { extension_hint: Some("png") }),
        (atoms[ImageRaw], TypeHint::Image { extension_hint: Some("raw") }),
        (atoms[ImageSvg], TypeHint::Image { extension_hint: Some("svg") }),
        (atoms[ImageTiff], TypeHint::Image { extension_hint: Some("tiff") }),
        (atoms[ImageWebp], TypeHint::Image { extension_hint: Some("webp") }),
        (atoms[ImageXIcon], TypeHint::Image { extension_hint: Some("ico") }),
    ]
}

impl TransferType for SelectionType {
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

impl DataTransfer for Selection {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> std::ops::ControlFlow<()>,
    ) {
        let _ = self.types.iter().map(|mime| mime as &dyn TransferType).try_for_each(func);
    }
}

impl Dnd {
    pub fn new(xconn: Arc<XConnection>) -> Self {
        Dnd { xconn, state: None }
    }

    pub fn state(&self) -> Option<&DragState> {
        self.state.as_ref()
    }

    pub fn state_mut(&mut self) -> Option<&mut DragState> {
        self.state.as_mut()
    }

    pub fn find_type_by_hint(&self, hint: TypeHint) -> Option<&SelectionType> {
        self.state.as_ref()?.types.iter().find(|haystack| haystack.hint() == Some(hint))
    }

    pub fn init_state(
        &mut self,
        version: c_long,
        source_window: xproto::Window,
        target_window: xproto::Window,
        types: Arc<[SelectionType]>,
    ) -> &DragState {
        self.state.insert(DragState {
            version,
            types,
            source_window,
            target_window,
            ..Default::default()
        })
    }

    pub unsafe fn send_finished(
        &self,
        this_window: xproto::Window,
        target_window: xproto::Window,
    ) -> Result<(), X11Error> {
        let atoms = self.xconn.atoms();
        let Some(state) = &self.state else {
            return Err(X11Error::UnexpectedNull(
                "Drag-and-drop state was not initialized (called `send_finished` before XdndEnter",
            ));
        };
        let (accepted, action) = accept_fields(
            state.chosen_action().and_then(|action| ActionAtoms::new(atoms).atom(action)),
        );
        self.xconn
            .send_client_msg(target_window, target_window, atoms[XdndFinished] as _, None, [
                this_window,
                accepted,
                action as _,
                0,
                0,
            ])?
            .ignore_error();

        Ok(())
    }

    /// Ends a drop: requests `DELETE` after a move with received data, else sends `XdndFinished`.
    pub fn finish_drop(
        &mut self,
        this_window: xproto::Window,
        source_window: xproto::Window,
        now: Instant,
    ) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        if !needs_delete(state.chosen_action(), state.received) {
            self.finish_now(this_window, source_window);
            return;
        }

        state.delete = Some(PendingDelete::new(this_window, source_window, now));
        let delete = self.xconn.atoms()[DELETE];
        self.convert_selection(this_window, self.xconn.timestamp(), delete);
    }

    /// Sends the `XdndFinished` an answered `DELETE` request waited for.
    ///
    /// Returns `false` when no `DELETE` request is outstanding.
    pub fn delete_answered(&mut self) -> bool {
        let Some(pending) = self.state.as_mut().and_then(|state| state.delete.take()) else {
            return false;
        };
        self.finish_now(pending.this_window, pending.source_window);
        true
    }

    /// The instant at which an outstanding `DELETE` request is given up.
    pub fn deadline(&self) -> Option<Instant> {
        self.state.as_ref()?.delete.map(|pending| pending.deadline)
    }

    /// Sends `XdndFinished` when the `DELETE` request is unanswered at `now`.
    pub fn tick(&mut self, now: Instant) {
        let expired = self
            .state
            .as_mut()
            .and_then(|state| state.delete.take_if(|pending| pending.expired(now)));
        if let Some(pending) = expired {
            warn!("The drag source did not answer DELETE; finishing the drop");
            self.finish_now(pending.this_window, pending.source_window);
        }
    }

    fn finish_now(&self, this_window: xproto::Window, source_window: xproto::Window) {
        // SAFETY: `send_finished` only sends a client message over the connection.
        if let Err(err) = unsafe { self.send_finished(this_window, source_window) } {
            warn!("Failed to send `XdndFinished`: {err}");
        }
    }

    pub unsafe fn get_type_list(
        &self,
        source_window: xproto::Window,
    ) -> Result<Vec<xproto::Atom>, util::GetPropertyError> {
        let atoms = self.xconn.atoms();
        self.xconn.get_property(
            source_window,
            atoms[XdndTypeList],
            xproto::Atom::from(xproto::AtomEnum::ATOM),
        )
    }

    pub fn convert_selection(
        &self,
        window: xproto::Window,
        time: xproto::Timestamp,
        new_type: xproto::Atom,
    ) {
        let atoms = self.xconn.atoms();
        self.xconn
            .xcb_connection()
            // TODO: We store the converted selection back to `XdndSelection`. We should store to
            // some new place so that `XdndSelection` remains untouched.
            .convert_selection(window, atoms[XdndSelection], new_type, atoms[XdndSelection], time)
            .expect_then_ignore_error("Failed to send XdndSelection event")
    }

    pub unsafe fn send_status(
        &self,
        this_window: xproto::Window,
        target_window: xproto::Window,
        action: Option<DndAction>,
    ) -> Result<(), X11Error> {
        let atoms = self.xconn.atoms();
        let (accepted, action) =
            accept_fields(action.and_then(|action| ActionAtoms::new(atoms).atom(action)));
        self.xconn
            .send_client_msg(target_window, target_window, atoms[XdndStatus] as _, None, [
                this_window,
                accepted,
                0,
                0,
                action as _,
            ])?
            .ignore_error();

        Ok(())
    }

    pub fn read_data(
        &self,
        window: xproto::Window,
        type_: SelectionType,
    ) -> Result<SelectionReader, util::GetPropertyError> {
        let atoms = self.xconn.atoms();
        let type_atom = type_.atom();
        let bytes = self.xconn.get_property(window, atoms[XdndSelection], type_atom)?;

        Ok(SelectionReader { type_, data: bytes })
    }
}

#[cfg(test)]
mod tests {
    use winit_core::event_loop::DndAction;

    use std::time::Instant;

    use super::{
        ActionAtoms, DELETE_TIMEOUT, PendingDelete, accept_fields, choose_action, needs_delete,
    };

    const ATOMS: ActionAtoms = ActionAtoms { copy: 10, move_: 11 };

    #[test]
    fn a_rejection_sends_zero_not_the_none_atom() {
        assert_eq!(accept_fields(None), (0, 0));
        assert_eq!(accept_fields(Some(11)), (1, 11));
    }

    #[test]
    fn only_a_move_with_received_data_deletes() {
        assert!(needs_delete(Some(DndAction::Move), true));
        assert!(!needs_delete(Some(DndAction::Move), false));
        assert!(!needs_delete(Some(DndAction::Copy), true));
        assert!(!needs_delete(None, true));
    }

    #[test]
    fn a_delete_request_expires() {
        let now = Instant::now();
        let pending = PendingDelete::new(1, 2, now);
        assert!(!pending.expired(now));
        assert!(pending.expired(now + DELETE_TIMEOUT));
    }

    #[test]
    fn atoms_map_to_actions() {
        assert_eq!(ATOMS.action(10), Some(DndAction::Copy));
        assert_eq!(ATOMS.action(11), Some(DndAction::Move));
        assert_eq!(ATOMS.action(0), None);
        assert_eq!(ATOMS.action(12), None);
    }

    #[test]
    fn actions_map_to_atoms() {
        assert_eq!(ATOMS.atom(DndAction::Copy), Some(10));
        assert_eq!(ATOMS.atom(DndAction::Move), Some(11));
        assert_eq!(ATOMS.atom(DndAction::Link), None);
        assert_eq!(ATOMS.atom(DndAction::Ask), None);
        assert_eq!(ATOMS.atom(DndAction::Private), None);
    }

    #[test]
    fn mapping_round_trips() {
        for action in [DndAction::Copy, DndAction::Move] {
            assert_eq!(ATOMS.atom(action).and_then(|atom| ATOMS.action(atom)), Some(action));
        }
    }

    #[test]
    fn requested_action_wins_when_valid() {
        let valid = [DndAction::Copy, DndAction::Move];
        assert_eq!(choose_action(Some(DndAction::Move), &valid), Some(DndAction::Move));
        assert_eq!(choose_action(Some(DndAction::Copy), &valid), Some(DndAction::Copy));
    }

    #[test]
    fn preference_order_does_not_override_the_source() {
        let valid = [DndAction::Move, DndAction::Copy];
        assert_eq!(choose_action(Some(DndAction::Copy), &valid), Some(DndAction::Copy));
    }

    #[test]
    fn falls_back_to_copy() {
        assert_eq!(choose_action(Some(DndAction::Move), &[DndAction::Copy]), Some(DndAction::Copy));
        assert_eq!(choose_action(None, &[DndAction::Move, DndAction::Copy]), Some(DndAction::Copy));
        assert_eq!(choose_action(Some(DndAction::Link), &[DndAction::Copy]), Some(DndAction::Copy));
    }

    #[test]
    fn rejects_without_a_match() {
        assert_eq!(choose_action(Some(DndAction::Move), &[]), None);
        assert_eq!(choose_action(None, &[]), None);
        assert_eq!(choose_action(Some(DndAction::Copy), &[DndAction::Move]), None);
        assert_eq!(choose_action(None, &[DndAction::Move]), None);
        assert_eq!(choose_action(Some(DndAction::Link), &[DndAction::Link]), None);
    }
}
