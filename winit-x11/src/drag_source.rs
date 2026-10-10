//! The XDND drag source: `start_drag` and the events of an outgoing drag.

use std::os::raw::{c_int, c_ulong};
use std::time::Instant;

use tracing::warn;
use winit_core::cursor::CursorIcon;
use winit_core::data_transfer::{DataTransferId, DataTransferSend, SendData, TransferType};
use winit_core::error::{NotSupportedError, RequestError};
use winit_core::event::WindowEvent;
use winit_core::event_loop::{DndAction, DragIcon};
use winit_core::icon::RgbaIcon;
use winit_core::window::WindowId;
use x11_dl::xlib;
use x11rb::connection::{Connection, RequestConnection};
use x11_dl::xinput2;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};

use crate::atoms::{
    _NET_WM_WINDOW_TYPE, _NET_WM_WINDOW_TYPE_DND, ATOM_PAIR, AtomName, DELETE, INCR, MULTIPLE, NULL,
    STRING,
    TARGETS, TIMESTAMP, XdndAware, XdndDrop, XdndEnter, XdndFinished, XdndLeave, XdndPosition,
    XdndProxy, XdndSelection, XdndStatus, XdndTypeList,
};
use crate::dnd::{ActionAtoms, SelectionType, next_transfer_id};
use crate::event_loop::{ActiveEventLoop, X11Error, mkwid};
use crate::xdnd_source::{
    self, EndedGrab, Feedback, Finished, FinishedData, Incr, Machine, Message, Outcome, Output,
    Pointer, Status, StatusData, Target,
};

/// The deepest window nesting searched for a target.
const MAX_TARGET_DEPTH: usize = 32;

/// The most targets accepted in one `MULTIPLE` request.
const MAX_MULTIPLE_PAIRS: usize = 64;

/// The largest property piece written at once.
const MAX_PIECE: usize = 1 << 18;

/// Bytes kept free in a request for its header.
const REQUEST_HEADER: usize = 64;

/// The keysym of Escape.
const ESCAPE_KEYSYM: u32 = 0xff1b;

/// The XI2 events of the pointer grab.
fn pointer_grab_events() -> u32 {
    u32::from(
        xinput::XIEventMask::MOTION
            | xinput::XIEventMask::BUTTON_PRESS
            | xinput::XIEventMask::BUTTON_RELEASE,
    )
}

/// The XI2 events of the keyboard grab.
fn keyboard_grab_events() -> u32 {
    u32::from(xinput::XIEventMask::KEY_PRESS | xinput::XIEventMask::KEY_RELEASE)
}

/// The master devices grabbed for a drag.
#[derive(Debug, Clone, Copy)]
struct Grab {
    pointer: xinput::DeviceId,
    keyboard: Option<xinput::DeviceId>,
}

/// An event for the application, addressed to a window.
pub(crate) type DragEvent = (WindowId, WindowEvent);

/// What the drag source did with an X event.
pub(crate) enum Handled {
    /// The event does not belong to the drag source.
    No,
    /// The event belongs to the drag source and is not processed further.
    Consumed(Option<DragEvent>),
    /// The event concerns the drag source and is processed further as usual.
    Observed(Option<DragEvent>),
}

/// The outgoing drag and the selection transfers still being served.
#[derive(Debug, Default)]
pub(crate) struct DragSources {
    drag: Option<OutgoingDrag>,
    incr: Vec<Incr>,
    ended_grab: Option<EndedGrab>,
}

#[derive(Debug)]
struct OutgoingDrag {
    id: DataTransferId,
    window: xproto::Window,
    grab: Grab,
    machine: Machine,
    send_data: Box<dyn DataTransferSend>,
    types: Vec<SelectionType>,
    owned_since: xproto::Timestamp,
    escape: Vec<u8>,
    icon: Option<IconWindow>,
}

/// How a window takes part in XDND.
enum Awareness {
    /// The window has no `XdndAware` property.
    Unaware,
    /// The window speaks only versions older than this source talks to.
    Unsupported,
    /// The window takes drops.
    Speaks(Target),
}

#[derive(Debug, Clone, Copy)]
struct IconWindow {
    window: xproto::Window,
    offset_x: i32,
    offset_y: i32,
}

fn window_from(value: c_ulong) -> Option<u32> {
    u32::try_from(value).ok()
}

fn time_from(value: c_ulong) -> u32 {
    u32::try_from(value & 0xffff_ffff).unwrap_or_default()
}

impl ActiveEventLoop {
    /// Starts an outgoing drag from `source`.
    pub(crate) fn begin_drag(
        &self,
        source: WindowId,
        send_data: Box<dyn DataTransferSend>,
        actions: &[DndAction],
        icon: Option<DragIcon>,
    ) -> Result<DataTransferId, RequestError> {
        let allowed = xdnd_source::supported_actions(actions);
        if allowed.is_empty() {
            return Err(NotSupportedError::new(
                "start_drag on X11 needs DndAction::Copy or DndAction::Move",
            )
            .into());
        }

        if self.drag_sources.borrow().drag.is_some() {
            return Err(os_error!("start_drag called while a drag is in progress").into());
        }

        let window = u32::try_from(source.into_raw())
            .map_err(|_| os_error!("start_drag called with an invalid window ID"))?;
        if !self.windows.borrow().contains_key(&source) {
            return Err(os_error!("start_drag called with an unknown window").into());
        }

        let atoms = self.xconn.atoms();
        let mut types: Vec<SelectionType> = Vec::new();
        send_data.for_each_available_type(&mut |type_| {
            for offered in SelectionType::offered_for(atoms, type_) {
                if !types.contains(&offered) {
                    types.push(offered);
                }
            }

            std::ops::ControlFlow::Continue(())
        });
        if types.is_empty() {
            return Err(NotSupportedError::new("start_drag offers no type X11 can transfer").into());
        }

        let owned_since = self.take_selection(window, &types).map_err(|err| os_error!(err))?;
        let grab = match self.grab_for_drag(window) {
            Ok(grab) => grab,
            Err(err) => {
                self.release_selection(window, owned_since);
                return Err(os_error!(err).into());
            },
        };

        let escape = self.escape_keycodes().unwrap_or_else(|err| {
            warn!("Escape cannot cancel the drag, the keyboard mapping is unavailable: {err}");
            Vec::new()
        });
        let icon = icon.and_then(|icon| match self.create_icon_window(&icon) {
            Ok(window) => window,
            Err(err) => {
                warn!("Failed to create the drag icon: {err}");
                None
            },
        });

        let id = next_transfer_id();
        let mut drag = OutgoingDrag {
            id,
            window,
            grab,
            machine: Machine::new(allowed),
            send_data,
            types,
            owned_since,
            escape,
            icon,
        };

        match self.query_pointer_state() {
            Ok((x, y, mask)) => {
                let outputs = self.pointer_moved(&mut drag, x, y, mask, owned_since);
                self.run_outputs(&mut drag, outputs);
            },
            Err(err) => warn!("Failed to query the pointer at the start of a drag: {err}"),
        }

        self.drag_sources.borrow_mut().drag = Some(drag);
        self.flush_drag();
        Ok(id)
    }

    /// Owns `XdndSelection` for `window` and announces `types` on it.
    fn take_selection(
        &self,
        window: xproto::Window,
        types: &[SelectionType],
    ) -> Result<xproto::Timestamp, X11Error> {
        let atoms = self.xconn.atoms();
        let conn = self.xconn.xcb_connection();
        let type_atoms: Vec<u32> = types.iter().map(SelectionType::atom).collect();
        self.xconn
            .change_property(
                window,
                atoms[XdndTypeList],
                u32::from(xproto::AtomEnum::ATOM),
                xproto::PropMode::REPLACE,
                &type_atoms,
            )?
            .ignore_error();

        let time = self.xconn.timestamp();
        conn.set_selection_owner(window, atoms[XdndSelection], time)?.ignore_error();
        let owner = conn.get_selection_owner(atoms[XdndSelection])?.reply()?.owner;
        if owner != window {
            return Err(X11Error::DragStart("another client owns XdndSelection"));
        }

        Ok(time)
    }

    fn release_selection(&self, window: xproto::Window, time: xproto::Timestamp) {
        let atoms = self.xconn.atoms();
        let conn = self.xconn.xcb_connection();
        let owner = conn
            .get_selection_owner(atoms[XdndSelection])
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.owner);
        if owner == Some(window) {
            match conn.set_selection_owner(x11rb::NONE, atoms[XdndSelection], time) {
                Ok(cookie) => cookie.ignore_error(),
                Err(err) => warn!("Failed to release XdndSelection: {err}"),
            }
        }
    }

    /// Grabs the client pointer and its paired keyboard on `window` with XI2 for the drag.
    fn grab_for_drag(&self, window: xproto::Window) -> Result<Grab, X11Error> {
        let conn = self.xconn.xcb_connection();
        let pointer = conn.xinput_xi_get_client_pointer(window)?.reply()?.deviceid;
        if !self.grab_device(window, pointer, Feedback::NoDrop)? {
            return Err(X11Error::DragStart("the XI2 pointer grab failed"));
        }

        let keyboard = match self.grab_keyboard(window, pointer) {
            Ok(Some(keyboard)) => Some(keyboard),
            Ok(None) => {
                warn!("Escape cannot cancel the drag, the keyboard grab was refused");
                None
            },
            Err(err) => {
                warn!("Escape cannot cancel the drag, the keyboard grab failed: {err}");
                None
            },
        };

        Ok(Grab { pointer, keyboard })
    }

    /// Grabs `pointer` on `window` showing the cursor for `feedback`, `false` when refused.
    fn grab_device(
        &self,
        window: xproto::Window,
        pointer: xinput::DeviceId,
        feedback: Feedback,
    ) -> Result<bool, X11Error> {
        let reply = self
            .xconn
            .xcb_connection()
            .xinput_xi_grab_device(
                window,
                x11rb::CURRENT_TIME,
                self.feedback_cursor(feedback),
                pointer,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                xinput::GrabOwner::NO_OWNER,
                &[pointer_grab_events()],
            )?
            .reply()?;
        Ok(reply.status == xproto::GrabStatus::SUCCESS)
    }

    /// Grabs the master keyboard paired with `pointer` on `window`.
    fn grab_keyboard(
        &self,
        window: xproto::Window,
        pointer: xinput::DeviceId,
    ) -> Result<Option<xinput::DeviceId>, X11Error> {
        let conn = self.xconn.xcb_connection();
        let devices = conn.xinput_xi_query_device(pointer)?.reply()?;
        let Some(keyboard) = devices
            .infos
            .iter()
            .find(|info| {
                info.deviceid == pointer && info.type_ == xinput::DeviceType::MASTER_POINTER
            })
            .map(|info| info.attachment)
        else {
            return Ok(None);
        };

        let reply = conn
            .xinput_xi_grab_device(
                window,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                keyboard,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                xinput::GrabOwner::NO_OWNER,
                &[keyboard_grab_events()],
            )?
            .reply()?;
        Ok((reply.status == xproto::GrabStatus::SUCCESS).then_some(keyboard))
    }

    fn feedback_cursor(&self, feedback: Feedback) -> xproto::Cursor {
        let icon = match feedback {
            Feedback::NoDrop => CursorIcon::NoDrop,
            Feedback::Copy => CursorIcon::Copy,
            Feedback::Move => CursorIcon::Move,
        };
        self.xconn.cached_cursor(Some(icon)).unwrap_or_else(|err| {
            warn!("Failed to load the {} cursor: {err}", icon.name());
            x11rb::NONE
        })
    }

    fn escape_keycodes(&self) -> Result<Vec<u8>, X11Error> {
        let setup = self.xconn.xcb_connection().setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let count = max.saturating_sub(min).saturating_add(1);
        let mapping = self.xconn.xcb_connection().get_keyboard_mapping(min, count)?.reply()?;
        Ok(xdnd_source::keycodes_for(
            &mapping.keysyms,
            mapping.keysyms_per_keycode,
            min,
            ESCAPE_KEYSYM,
        ))
    }

    fn query_pointer_state(&self) -> Result<(i16, i16, u16), X11Error> {
        let reply = self.xconn.xcb_connection().query_pointer(self.root)?.reply()?;
        Ok((reply.root_x, reply.root_y, u16::from(reply.mask)))
    }

    /// Creates the override-redirect window that shows the drag icon.
    fn create_icon_window(&self, icon: &DragIcon) -> Result<Option<IconWindow>, X11Error> {
        let Some(rgba) = icon.icon.cast_ref::<RgbaIcon>() else {
            warn!("DragIcon::icon must be an RgbaIcon on X11; ignoring");
            return Ok(None);
        };
        let (Ok(width), Ok(height)) = (u16::try_from(rgba.width()), u16::try_from(rgba.height()))
        else {
            warn!("The drag icon is larger than X11 allows; ignoring");
            return Ok(None);
        };
        if width == 0 || height == 0 {
            return Ok(None);
        }

        let conn = self.xconn.xcb_connection();
        if conn.extension_information(shape::X11_EXTENSION_NAME)?.is_none() {
            warn!("The drag icon needs the SHAPE extension; ignoring");
            return Ok(None);
        }

        let screen = self.xconn.default_root();
        let argb = screen
            .allowed_depths
            .iter()
            .filter(|depth| depth.depth == 32)
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.class == xproto::VisualClass::TRUE_COLOR)
            .map(|visual| visual.visual_id);
        let (depth, visual, colormap) = match argb {
            Some(visual) => {
                let colormap = conn.generate_id()?;
                conn.create_colormap(xproto::ColormapAlloc::NONE, colormap, screen.root, visual)?
                    .ignore_error();
                (32, visual, colormap)
            },
            None if screen.root_depth == 24 => (24, screen.root_visual, screen.default_colormap),
            None => {
                warn!("The drag icon needs a 24 or 32 bit visual; ignoring");
                return Ok(None);
            },
        };

        let window = conn.generate_id()?;
        let attributes = xproto::CreateWindowAux::new()
            .override_redirect(1)
            .border_pixel(0)
            .background_pixel(0)
            .colormap(colormap);
        conn.create_window(
            depth,
            window,
            screen.root,
            0,
            0,
            width,
            height,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            visual,
            &attributes,
        )?
        .ignore_error();
        if depth == 32 {
            conn.free_colormap(colormap)?.ignore_error();
        }

        let atoms = self.xconn.atoms();
        self.xconn
            .change_property(
                window,
                atoms[_NET_WM_WINDOW_TYPE],
                u32::from(xproto::AtomEnum::ATOM),
                xproto::PropMode::REPLACE,
                &[atoms[_NET_WM_WINDOW_TYPE_DND]],
            )?
            .ignore_error();

        self.shape_icon_window(window, rgba.buffer(), width)?;
        self.paint_icon_window(window, depth, width, height, rgba.buffer())?;
        conn.map_window(window)?.ignore_error();

        Ok(Some(IconWindow { window, offset_x: icon.offset_x, offset_y: icon.offset_y }))
    }

    /// Lets input pass through the icon window and clips it to the visible pixels.
    fn shape_icon_window(
        &self,
        window: xproto::Window,
        rgba: &[u8],
        width: u16,
    ) -> Result<(), X11Error> {
        let conn = self.xconn.xcb_connection();
        for kind in [shape::SK::INPUT, shape::SK::BOUNDING] {
            conn.shape_rectangles(
                shape::SO::SET,
                kind,
                xproto::ClipOrdering::UNSORTED,
                window,
                0,
                0,
                &[],
            )?
            .ignore_error();
        }

        let rectangles: Vec<xproto::Rectangle> = xdnd_source::opaque_runs(rgba, width)
            .into_iter()
            .filter_map(|run| {
                Some(xproto::Rectangle {
                    x: i16::try_from(run.x).ok()?,
                    y: i16::try_from(run.y).ok()?,
                    width: run.width,
                    height: 1,
                })
            })
            .collect();
        let per_request = self.request_room() / 8;
        for chunk in rectangles.chunks(per_request.max(1)) {
            conn.shape_rectangles(
                shape::SO::UNION,
                shape::SK::BOUNDING,
                xproto::ClipOrdering::YX_SORTED,
                window,
                0,
                0,
                chunk,
            )?
            .ignore_error();
        }

        Ok(())
    }

    /// Sets the icon pixels as the background of the icon window.
    fn paint_icon_window(
        &self,
        window: xproto::Window,
        depth: u8,
        width: u16,
        height: u16,
        rgba: &[u8],
    ) -> Result<(), X11Error> {
        let conn = self.xconn.xcb_connection();
        let pixels = xdnd_source::premultiplied_argb(rgba, self.xconn.needs_endian_swap());
        let row_bytes = usize::from(width).saturating_mul(4);
        let rows_per_request = self.request_room() / row_bytes.max(1);
        if rows_per_request == 0 {
            warn!("The drag icon rows are too wide for one request; ignoring the pixels");
            return Ok(());
        }

        let pixmap = xproto::PixmapWrapper::create_pixmap(conn, depth, window, width, height)?;
        let gc = xproto::GcontextWrapper::create_gc(
            conn,
            pixmap.pixmap(),
            &xproto::CreateGCAux::default(),
        )?;
        for (index, rows) in pixels.chunks(rows_per_request.saturating_mul(row_bytes)).enumerate() {
            let top = index.saturating_mul(rows_per_request);
            let (Ok(top), Ok(count)) =
                (i16::try_from(top), u16::try_from(rows.len() / row_bytes.max(1)))
            else {
                break;
            };
            conn.put_image(
                xproto::ImageFormat::Z_PIXMAP,
                pixmap.pixmap(),
                gc.gcontext(),
                width,
                count,
                0,
                top,
                0,
                depth,
                rows,
            )?
            .ignore_error();
        }

        conn.change_window_attributes(
            window,
            &xproto::ChangeWindowAttributesAux::new().background_pixmap(pixmap.pixmap()),
        )?
        .ignore_error();
        Ok(())
    }

    /// The bytes a request may carry besides its header.
    fn request_room(&self) -> usize {
        self.xconn
            .xcb_connection()
            .maximum_request_bytes()
            .saturating_sub(REQUEST_HEADER)
            .min(MAX_PIECE)
    }

    /// The XDND target under the root point `(x, y)`.
    fn find_target(&self, x: i16, y: i16) -> Option<Target> {
        let conn = self.xconn.xcb_connection();
        let mut window = self.root;
        for _ in 0..MAX_TARGET_DEPTH {
            let reply = conn.translate_coordinates(self.root, window, x, y).ok()?.reply().ok()?;
            if reply.child == x11rb::NONE {
                break;
            }

            window = reply.child;
            match self.xdnd_target(window) {
                Awareness::Unaware => {},
                Awareness::Unsupported => return None,
                Awareness::Speaks(target) => return Some(target),
            }
        }

        match self.xdnd_target(self.root) {
            Awareness::Speaks(target) => Some(target),
            Awareness::Unaware | Awareness::Unsupported => None,
        }
    }

    /// Whether `window` takes XDND drops and in which version.
    fn xdnd_target(&self, window: xproto::Window) -> Awareness {
        let atoms = self.xconn.atoms();
        let window_type = u32::from(xproto::AtomEnum::WINDOW);
        let property = |window: xproto::Window, name: AtomName| -> Vec<u32> {
            self.xconn.get_property(window, atoms[name], window_type).unwrap_or_default()
        };
        let proxy = property(window, XdndProxy);
        let proxy = proxy
            .first()
            .and_then(|proxy| xdnd_source::valid_proxy(&[*proxy], &property(*proxy, XdndProxy)))
            .unwrap_or(window);
        let aware: Vec<u32> = self
            .xconn
            .get_property(proxy, atoms[XdndAware], u32::from(xproto::AtomEnum::ATOM))
            .unwrap_or_default();
        if aware.is_empty() {
            return Awareness::Unaware;
        }

        match xdnd_source::negotiate_version(&aware) {
            Some(version) => Awareness::Speaks(Target { window, proxy, version }),
            None => Awareness::Unsupported,
        }
    }

    fn move_icon(&self, icon: IconWindow, x: i16, y: i16) {
        let left = i32::from(x).saturating_add(icon.offset_x);
        let top = i32::from(y).saturating_add(icon.offset_y);
        let aux = xproto::ConfigureWindowAux::new()
            .x(left)
            .y(top)
            .stack_mode(xproto::StackMode::ABOVE);
        match self.xconn.xcb_connection().configure_window(icon.window, &aux) {
            Ok(cookie) => cookie.ignore_error(),
            Err(err) => warn!("Failed to move the drag icon: {err}"),
        }
    }

    fn pointer_moved(
        &self,
        drag: &mut OutgoingDrag,
        x: i16,
        y: i16,
        mask: u16,
        time: u32,
    ) -> Vec<Output> {
        if let Some(icon) = drag.icon {
            self.move_icon(icon, x, y);
        }

        let Some(action) = xdnd_source::source_action(drag.machine.allowed(), mask) else {
            return Vec::new();
        };
        if !drag.machine.is_dragging() {
            return Vec::new();
        }

        let target = self.find_target(x, y);
        drag.machine.motion(Instant::now(), target, Pointer { x, y, time, action })
    }

    fn send_message(&self, drag: &OutgoingDrag, target: Target, message: Message) {
        let atoms = self.xconn.atoms();
        let source = drag.window;
        let (type_, data) = match message {
            Message::Enter => {
                let types: Vec<u32> = drag.types.iter().map(SelectionType::atom).collect();
                (atoms[XdndEnter], xdnd_source::enter_data(source, target.version, &types).0)
            },
            Message::Position(pointer) => {
                let action = ActionAtoms::new(atoms).atom(pointer.action).unwrap_or_default();
                let data =
                    xdnd_source::position_data(source, pointer.x, pointer.y, pointer.time, action);
                (atoms[XdndPosition], data)
            },
            Message::Leave => (atoms[XdndLeave], xdnd_source::leave_data(source)),
            Message::Drop { time } => (atoms[XdndDrop], xdnd_source::drop_data(source, time)),
        };
        let event = xproto::ClientMessageEvent::new(32, target.window, type_, data);
        let sent = self.xconn.xcb_connection().send_event(
            false,
            target.proxy,
            xproto::EventMask::NO_EVENT,
            event,
        );
        match sent {
            Ok(cookie) => cookie.ignore_error(),
            Err(err) => warn!("Failed to send an XDND message: {err}"),
        }
    }

    /// Carries out what the machine asked for and returns the event that ends the drag.
    fn run_outputs(&self, drag: &mut OutgoingDrag, outputs: Vec<Output>) -> Option<DragEvent> {
        let mut event = None;
        for output in outputs {
            match output {
                Output::Send(target, message) => self.send_message(drag, target, message),
                Output::Cursor(feedback) => {
                    match self.grab_device(drag.window, drag.grab.pointer, feedback) {
                        Ok(true) => {},
                        Ok(false) => warn!("The pointer grab refused the new drag cursor"),
                        Err(err) => warn!("Failed to change the drag cursor: {err}"),
                    }
                },
                Output::Finish(outcome) => {
                    let id = drag.id;
                    let window_event = match outcome {
                        Outcome::Dropped(action) => WindowEvent::OutgoingDragDropped { id, action },
                        Outcome::Canceled => WindowEvent::OutgoingDragCanceled { id },
                    };
                    event = Some((mkwid(drag.window), window_event));
                },
            }
        }

        event
    }

    /// Releases everything an ended drag holds.
    fn end_drag(&self, drag: &OutgoingDrag) {
        let conn = self.xconn.xcb_connection();
        let devices = std::iter::once(drag.grab.pointer).chain(drag.grab.keyboard);
        for device in devices {
            match conn.xinput_xi_ungrab_device(x11rb::CURRENT_TIME, device) {
                Ok(cookie) => cookie.ignore_error(),
                Err(err) => warn!("Failed to release the drag grab of device {device}: {err}"),
            }
        }

        match conn.delete_property(drag.window, self.xconn.atoms()[XdndTypeList]) {
            Ok(cookie) => cookie.ignore_error(),
            Err(err) => warn!("Failed to delete XdndTypeList after the drag: {err}"),
        }

        if let Some(icon) = drag.icon {
            match conn.destroy_window(icon.window) {
                Ok(cookie) => cookie.ignore_error(),
                Err(err) => warn!("Failed to destroy the drag icon: {err}"),
            }
        }

        self.release_selection(drag.window, drag.owned_since);
    }

    fn flush_drag(&self) {
        if let Err(err) = self.xconn.xcb_connection().flush() {
            warn!("Failed to flush the drag requests: {err}");
        }
    }

    /// Runs `step` on the drag and ends the drag once the machine is done.
    fn with_drag<F>(&self, step: F) -> Option<DragEvent>
    where
        F: FnOnce(&Self, &mut OutgoingDrag) -> Vec<Output>,
    {
        let mut sources = self.drag_sources.borrow_mut();
        let drag = sources.drag.as_mut()?;
        let outputs = step(self, drag);
        let event = self.run_outputs(drag, outputs);
        if drag.machine.is_done() {
            if let Some(drag) = sources.drag.take() {
                self.end_drag(&drag);
                let until = xdnd_source::after(Instant::now(), xdnd_source::UNGRAB_TIMEOUT);
                sources.ended_grab = Some(EndedGrab { window: drag.window, until });
            }
        }

        drop(sources);
        self.flush_drag();
        event
    }

    /// The next instant at which [`ActiveEventLoop::drag_tick`] has work to do.
    pub(crate) fn drag_deadline(&self) -> Option<Instant> {
        let sources = self.drag_sources.borrow();
        let drag = sources.drag.as_ref().and_then(|drag| drag.machine.deadline());
        let incr = sources.incr.iter().map(|incr| incr.deadline).min();
        let delete = self.dnd.borrow().deadline();
        [drag, incr, delete].into_iter().flatten().min()
    }

    /// Handles the drag timeouts that expired at `now`.
    pub(crate) fn drag_tick(&self, now: Instant) -> Option<DragEvent> {
        let expired: Vec<Incr> = {
            let mut sources = self.drag_sources.borrow_mut();
            let (expired, kept): (Vec<Incr>, Vec<Incr>) =
                std::mem::take(&mut sources.incr).into_iter().partition(|incr| now >= incr.deadline);
            sources.incr = kept;
            expired
        };
        for incr in &expired {
            warn!("An INCR selection transfer timed out");
            self.stop_property_events(incr.requestor);
        }

        self.dnd.borrow_mut().tick(now);
        self.with_drag(|_, drag| drag.machine.tick(now))
    }

    fn stop_property_events(&self, requestor: xproto::Window) {
        let aux = xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::NO_EVENT);
        match self.xconn.xcb_connection().change_window_attributes(requestor, &aux) {
            Ok(cookie) => cookie.ignore_error(),
            Err(err) => warn!("Failed to stop property events: {err}"),
        }
    }

    fn is_drag_window(&self, window: Option<u32>) -> bool {
        let sources = self.drag_sources.borrow();
        sources.drag.as_ref().is_some_and(|drag| Some(drag.window) == window)
    }

    /// Hands an X event to the drag source.
    pub(crate) fn drag_event(&self, xev: &xlib::XEvent) -> Handled {
        match xev.get_type() {
            xlib::ClientMessage => self.drag_client_message(xev.as_ref()),
            xlib::SelectionRequest => self.selection_request(xev.as_ref()),
            xlib::SelectionClear => self.selection_clear(xev.as_ref()),
            xlib::PropertyNotify => self.incr_property(xev.as_ref()),
            xlib::DestroyNotify => {
                let event: &xlib::XDestroyWindowEvent = xev.as_ref();
                self.source_gone(window_from(event.window))
            },
            xlib::UnmapNotify => {
                let event: &xlib::XUnmapEvent = xev.as_ref();
                self.source_gone(window_from(event.window))
            },
            _ => Handled::No,
        }
    }

    fn source_gone(&self, window: Option<u32>) -> Handled {
        if !self.is_drag_window(window) {
            return Handled::No;
        }

        Handled::Observed(self.with_drag(|_, drag| drag.machine.cancel()))
    }

    /// Hands an XI2 device event of the drag grab to the drag source.
    ///
    /// Motion, button and key events on the grab window are consumed while a drag runs.
    pub(crate) fn drag_device_event(
        &self,
        evtype: c_int,
        event: &xinput2::XIDeviceEvent,
    ) -> Handled {
        if !self.is_drag_window(window_from(event.event)) {
            return Handled::No;
        }

        let time = time_from(event.time);
        self.xconn.set_timestamp(time);
        match evtype {
            xinput2::XI_Motion => {
                Handled::Consumed(self.with_drag(|this, drag| this.pointer_update(drag, time)))
            },
            xinput2::XI_ButtonPress => Handled::Consumed(None),
            xinput2::XI_ButtonRelease => Handled::Consumed(
                self.with_drag(|_, drag| drag.machine.release(Instant::now(), time)),
            ),
            xinput2::XI_KeyPress | xinput2::XI_KeyRelease => {
                let keycode = u8::try_from(event.detail).ok();
                let pressed = evtype == xinput2::XI_KeyPress;
                Handled::Consumed(self.with_drag(|this, drag| {
                    if pressed && keycode.is_some_and(|keycode| drag.escape.contains(&keycode)) {
                        return drag.machine.cancel();
                    }

                    this.pointer_update(drag, time)
                }))
            },
            _ => Handled::No,
        }
    }

    /// Whether an XI2 crossing or focus event stems from the drag grab and is dropped.
    pub(crate) fn drag_crossing_event(&self, event: &xinput2::XIEnterEvent) -> bool {
        let Some(window) = window_from(event.event) else {
            return false;
        };

        let sources = self.drag_sources.borrow();
        let active = sources.drag.as_ref().map(|drag| drag.window);
        xdnd_source::from_drag_grab(event.mode, window, active, sources.ended_grab, Instant::now())
    }

    /// Feeds the current pointer position and modifiers into the drag.
    fn pointer_update(&self, drag: &mut OutgoingDrag, time: u32) -> Vec<Output> {
        match self.query_pointer_state() {
            Ok((x, y, mask)) => self.pointer_moved(drag, x, y, mask, time),
            Err(err) => {
                warn!("Failed to query the pointer during a drag: {err}");
                Vec::new()
            },
        }
    }

    fn drag_client_message(&self, event: &xlib::XClientMessageEvent) -> Handled {
        let atoms = self.xconn.atoms();
        let Some(message_type) = window_from(event.message_type) else {
            return Handled::No;
        };
        if message_type != atoms[XdndStatus] && message_type != atoms[XdndFinished] {
            return Handled::No;
        }

        if !self.is_drag_window(window_from(event.window)) {
            return Handled::No;
        }

        let longs = event.data.as_longs();
        let mut data = [0u32; 5];
        for (slot, long) in data.iter_mut().zip(longs) {
            *slot = xdnd_source::card32(*long);
        }

        let actions = ActionAtoms::new(atoms);
        let event = if message_type == atoms[XdndStatus] {
            let status = StatusData::parse(data);
            let status = Status {
                window: status.window,
                accept: status.accept,
                want_position: status.want_position,
                rect: status.rect,
                action: actions.action(status.action),
            };
            self.with_drag(|_, drag| drag.machine.status(Instant::now(), status))
        } else {
            let finished = FinishedData::parse(data);
            let finished = Finished {
                window: finished.window,
                success: finished.success,
                action: actions.action(finished.action),
            };
            self.with_drag(|_, drag| drag.machine.finished(finished))
        };
        Handled::Consumed(event)
    }

    fn selection_clear(&self, event: &xlib::XSelectionClearEvent) -> Handled {
        let atoms = self.xconn.atoms();
        if window_from(event.selection) != Some(atoms[XdndSelection])
            || !self.is_drag_window(window_from(event.window))
        {
            return Handled::No;
        }

        Handled::Consumed(self.with_drag(|_, drag| drag.machine.cancel()))
    }

    fn selection_request(&self, event: &xlib::XSelectionRequestEvent) -> Handled {
        let atoms = self.xconn.atoms();
        let (Some(requestor), Some(selection), Some(target), Some(property)) = (
            window_from(event.requestor),
            window_from(event.selection),
            window_from(event.target),
            window_from(event.property),
        ) else {
            return Handled::No;
        };
        if selection != atoms[XdndSelection] {
            return Handled::No;
        }

        let time = time_from(event.time);
        let property = if property == x11rb::NONE { target } else { property };
        let converted = {
            let mut sources = self.drag_sources.borrow_mut();
            let DragSources { drag, incr, .. } = &mut *sources;
            match drag.as_mut() {
                Some(drag) if window_from(event.owner) == Some(drag.window) => {
                    xdnd_source::request_in_time(drag.owned_since, time)
                        && if target == atoms[DELETE] {
                            drag.machine.delete()
                                && self.write_property(requestor, property, atoms[NULL], &[])
                        } else {
                            self.convert(drag, incr, requestor, target, property)
                        }
                },
                _ => false,
            }
        };

        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time,
            requestor,
            selection,
            target,
            property: if converted { property } else { x11rb::NONE },
        };
        match self.xconn.xcb_connection().send_event(
            false,
            requestor,
            xproto::EventMask::NO_EVENT,
            notify,
        ) {
            Ok(cookie) => cookie.ignore_error(),
            Err(err) => warn!("Failed to answer a selection request: {err}"),
        }

        self.flush_drag();
        Handled::Consumed(None)
    }

    /// Converts the selection to `target` into `property` on `requestor`.
    fn convert(
        &self,
        drag: &OutgoingDrag,
        incr: &mut Vec<Incr>,
        requestor: xproto::Window,
        target: xproto::Atom,
        property: xproto::Atom,
    ) -> bool {
        let atoms = self.xconn.atoms();
        if target == atoms[TARGETS] {
            let mut list = vec![atoms[TARGETS], atoms[TIMESTAMP], atoms[MULTIPLE]];
            list.extend(drag.types.iter().map(SelectionType::atom));
            let atom = u32::from(xproto::AtomEnum::ATOM);
            return self.write_property(requestor, property, atom, &list);
        }

        if target == atoms[TIMESTAMP] {
            let integer = u32::from(xproto::AtomEnum::INTEGER);
            return self.write_property(requestor, property, integer, &[drag.owned_since]);
        }

        if target == atoms[MULTIPLE] {
            return self.convert_multiple(drag, incr, requestor, property);
        }

        self.convert_data(drag, incr, requestor, target, property)
    }

    fn convert_multiple(
        &self,
        drag: &OutgoingDrag,
        incr: &mut Vec<Incr>,
        requestor: xproto::Window,
        property: xproto::Atom,
    ) -> bool {
        let atoms = self.xconn.atoms();
        let Ok(mut pairs) = self.xconn.get_property::<u32>(requestor, property, atoms[ATOM_PAIR])
        else {
            return false;
        };
        if pairs.len() > MAX_MULTIPLE_PAIRS * 2 {
            return false;
        }

        for pair in pairs.chunks_exact_mut(2) {
            let &mut [target, ref mut pair_property] = pair else {
                continue;
            };
            let done = target != atoms[MULTIPLE]
                && *pair_property != x11rb::NONE
                && self.convert(drag, incr, requestor, target, *pair_property);
            if !done {
                *pair_property = x11rb::NONE;
            }
        }

        self.write_property(requestor, property, atoms[ATOM_PAIR], &pairs)
    }

    fn convert_data(
        &self,
        drag: &OutgoingDrag,
        incr: &mut Vec<Incr>,
        requestor: xproto::Window,
        target: xproto::Atom,
        property: xproto::Atom,
    ) -> bool {
        let atoms = self.xconn.atoms();
        let Some(type_) = drag.types.iter().find(|type_| type_.atom() == target) else {
            return false;
        };
        let type_: &dyn TransferType = type_;
        let Some(data) = drag.send_data.data_for_type(type_) else {
            return false;
        };
        let bytes = match data {
            SendData::Uris(uris) => Some(xdnd_source::encode_uri_list(&uris)),
            SendData::String(text) if target == atoms[STRING] => xdnd_source::encode_latin1(&text),
            SendData::String(text) => Some(text.into_bytes()),
            SendData::Bytes(bytes) => Some(bytes),
            _ => None,
        };
        let Some(bytes) = bytes else {
            return false;
        };

        let room = self.request_room();
        let own = self.windows.borrow().contains_key(&mkwid(requestor));
        if bytes.len() <= room || own {
            let limit =
                self.xconn.xcb_connection().maximum_request_bytes().saturating_sub(REQUEST_HEADER);
            return bytes.len() <= limit && self.write_bytes(requestor, property, target, &bytes);
        }

        if incr.len() >= xdnd_source::MAX_INCR_TRANSFERS {
            warn!("Too many INCR selection transfers at once; refusing another");
            return false;
        }

        let transfer = Incr::new(requestor, property, target, bytes, room, Instant::now());
        let Ok(length) = u32::try_from(transfer.len()) else {
            return false;
        };
        let aux =
            xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE);
        match self.xconn.xcb_connection().change_window_attributes(requestor, &aux) {
            Ok(cookie) => cookie.ignore_error(),
            Err(err) => {
                warn!("Failed to watch the requestor of an INCR transfer: {err}");
                return false;
            },
        }

        if !self.write_property(requestor, property, atoms[INCR], &[length]) {
            return false;
        }

        incr.push(transfer);
        true
    }

    fn write_property(
        &self,
        window: xproto::Window,
        property: xproto::Atom,
        type_: xproto::Atom,
        values: &[u32],
    ) -> bool {
        match self.xconn.change_property(window, property, type_, xproto::PropMode::REPLACE, values)
        {
            Ok(cookie) => {
                cookie.ignore_error();
                true
            },
            Err(err) => {
                warn!("Failed to write a selection property: {err}");
                false
            },
        }
    }

    fn write_bytes(
        &self,
        window: xproto::Window,
        property: xproto::Atom,
        type_: xproto::Atom,
        bytes: &[u8],
    ) -> bool {
        match self.xconn.change_property(window, property, type_, xproto::PropMode::REPLACE, bytes) {
            Ok(cookie) => {
                cookie.ignore_error();
                true
            },
            Err(err) => {
                warn!("Failed to write selection data: {err}");
                false
            },
        }
    }

    fn incr_property(&self, event: &xlib::XPropertyEvent) -> Handled {
        if event.state != xlib::PropertyDelete {
            return Handled::No;
        }

        let (Some(window), Some(atom)) = (window_from(event.window), window_from(event.atom))
        else {
            return Handled::No;
        };

        let mut sources = self.drag_sources.borrow_mut();
        let Some(index) =
            sources.incr.iter().position(|incr| incr.requestor == window && incr.property == atom)
        else {
            return Handled::No;
        };

        let now = Instant::now();
        let ended = match sources.incr.get_mut(index) {
            Some(incr) => {
                let (requestor, property, type_) = (incr.requestor, incr.property, incr.type_);
                match incr.next_piece(now) {
                    Some(piece) => {
                        let written = self.write_bytes(requestor, property, type_, piece);
                        !written || piece.is_empty()
                    },
                    None => true,
                }
            },
            None => false,
        };
        if ended {
            let incr = sources.incr.swap_remove(index);
            self.stop_property_events(incr.requestor);
        }

        drop(sources);
        self.flush_drag();
        Handled::Consumed(None)
    }
}
