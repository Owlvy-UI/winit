//! File drag and drop for the Wayland backend.
//!
//! Only the receiving side is handled. An offer is accepted when it advertises
//! `text/uri-list`, and the `file://` URIs it carries are reported through
//! `WindowEvent::HoveredFile`, `WindowEvent::DroppedFile` and
//! `WindowEvent::HoveredFileCancelled`, one event per file, like the other backends.

use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use percent_encoding::percent_decode;
use tracing::warn;

use sctk::data_device_manager::data_device::{DataDeviceData, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::DataSourceHandler;
use sctk::data_device_manager::WritePipe;
use sctk::reexports::calloop::{PostAction, RegistrationToken};
use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};

use crate::dpi::{LogicalPosition, PhysicalPosition};
use crate::event::WindowEvent;
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{make_wid, DeviceId, WindowId};

/// The only MIME type that is accepted.
const TEXT_URI_LIST: &str = "text/uri-list";

/// Upper bound for the size of a received URI list.
const MAX_URI_LIST_LEN: usize = 4 * 1024 * 1024;

/// Size of a single read from the transfer pipe.
const READ_CHUNK_LEN: usize = 64 * 1024;

/// Which part of the drag a transfer belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Data requested on enter, reported as `HoveredFile`.
    Hover,
    /// Data requested on drop, reported as `DroppedFile`.
    Drop,
}

/// The drag that is currently over one of our windows.
#[derive(Debug)]
struct ActiveDrag {
    /// Identifies the drag inside transfer callbacks.
    generation: u64,
    offer: DragOffer,
    data_device: ObjectId,
    window_id: WindowId,
    /// Last pointer position in surface local logical coordinates.
    position: (f64, f64),
    /// Whether `HoveredFile` was sent for this drag.
    hovered: bool,
    /// Whether the drop happened.
    dropped: bool,
    /// The transfer in progress, if any.
    transfer: Option<RegistrationToken>,
}

/// Drag and drop state of the event loop.
#[derive(Debug, Default)]
pub struct DndState {
    active: Option<ActiveDrag>,
    next_generation: u64,
}

impl WinitState {
    /// Sends `CursorMoved` for a drag at surface local logical `(x, y)` over `window_id`.
    fn dnd_report_position(&mut self, window_id: WindowId, (x, y): (f64, f64)) {
        let scale_factor = match self.windows.borrow().get(&window_id).map(|window| window.lock()) {
            Some(Ok(window)) => window.scale_factor(),
            Some(Err(_)) => {
                warn!("Window state is poisoned, dropping the drag position");
                return;
            },
            None => return,
        };

        let device_id = crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId));
        let position = drag_position(x, y, scale_factor);
        self.events_sink
            .push_window_event(WindowEvent::CursorMoved { device_id, position }, window_id);
    }

    /// Ends the active drag without a drop.
    fn dnd_cancel_active(&mut self) {
        let Some(drag) = self.dnd_state.active.take() else {
            return;
        };

        if let Some(token) = drag.transfer {
            self.loop_handle.remove(token);
        }

        self.events_sink.push_window_event(WindowEvent::HoveredFileCancelled, drag.window_id);
    }

    /// Starts reading the URI list of the active drag.
    ///
    /// Returns `false` when no transfer could be started.
    fn dnd_start_transfer(&mut self, phase: Phase) -> bool {
        let Some(drag) = self.dnd_state.active.as_mut() else {
            return false;
        };

        if let Some(token) = drag.transfer.take() {
            self.loop_handle.remove(token);
        }

        let pipe = match drag.offer.receive(TEXT_URI_LIST.to_owned()) {
            Ok(pipe) => pipe,
            Err(err) => {
                warn!("Failed to receive drag and drop data: {err}");
                return false;
            },
        };

        let generation = drag.generation;
        let mut buffer = Vec::new();
        let result = self.loop_handle.insert_source(pipe, move |_, file, state| {
            let mut chunk = [0u8; READ_CHUNK_LEN];
            loop {
                match (&**file).read(&mut chunk) {
                    Ok(0) => {
                        let data = std::mem::take(&mut buffer);
                        state.dnd_transfer_done(generation, phase, Some(&data));
                        return PostAction::Remove;
                    },
                    Ok(len) => {
                        let Some(read) = chunk.get(..len) else {
                            state.dnd_transfer_done(generation, phase, None);
                            return PostAction::Remove;
                        };

                        if buffer.len().saturating_add(len) > MAX_URI_LIST_LEN {
                            warn!("Drag and drop data exceeds {MAX_URI_LIST_LEN} bytes, ignoring");
                            state.dnd_transfer_done(generation, phase, None);
                            return PostAction::Remove;
                        }

                        buffer.extend_from_slice(read);
                        // Hand control back to the loop after every chunk.
                        return PostAction::Continue;
                    },
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(err) if err.kind() == ErrorKind::WouldBlock => return PostAction::Continue,
                    Err(err) => {
                        warn!("Failed to read drag and drop data: {err}");
                        state.dnd_transfer_done(generation, phase, None);
                        return PostAction::Remove;
                    },
                }
            }
        });

        match result {
            Ok(token) => {
                if let Some(drag) = self.dnd_state.active.as_mut() {
                    drag.transfer = Some(token);
                }
                true
            },
            Err(err) => {
                warn!("Failed to register drag and drop transfer: {}", err.error);
                false
            },
        }
    }

    /// Handles the end of a transfer. `data` is `None` when the transfer failed.
    fn dnd_transfer_done(&mut self, generation: u64, phase: Phase, data: Option<&[u8]>) {
        // Wake the user for the events below.
        self.dispatched_events = true;

        let Some(drag) = self.dnd_state.active.as_mut() else {
            return;
        };

        if drag.generation != generation {
            return;
        }

        drag.transfer = None;
        let paths = data.map(parse_uri_list).unwrap_or_default();
        let window_id = drag.window_id;
        let window_alive = self.windows.borrow().contains_key(&window_id);

        match phase {
            Phase::Hover => {
                if drag.dropped || drag.hovered || !window_alive {
                    return;
                }

                drag.hovered = !paths.is_empty();
                for path in paths {
                    self.events_sink.push_window_event(WindowEvent::HoveredFile(path), window_id);
                }
            },
            Phase::Drop => {
                let Some(drag) = self.dnd_state.active.take() else {
                    return;
                };

                if window_alive {
                    if paths.is_empty() {
                        if drag.hovered {
                            self.events_sink
                                .push_window_event(WindowEvent::HoveredFileCancelled, window_id);
                        }
                    } else {
                        self.dnd_report_position(window_id, drag.position);
                        for path in paths {
                            self.events_sink
                                .push_window_event(WindowEvent::DroppedFile(path), window_id);
                        }
                    }
                }

                if data.is_some() {
                    drag.offer.finish();
                }
                drag.offer.destroy();
            },
        }
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
        surface: &WlSurface,
    ) {
        // A new enter replaces whatever drag was active before.
        if self.dnd_state.active.as_ref().is_some_and(|drag| !drag.dropped) {
            self.dnd_cancel_active();
        }

        let Some(offer) = data_device.data::<DataDeviceData>().and_then(|d| d.drag_offer()) else {
            return;
        };

        let window_id = make_wid(surface);
        let is_window = self.windows.borrow().contains_key(&window_id);
        let has_uri_list = offer.with_mime_types(|types| types.iter().any(|t| t == TEXT_URI_LIST));

        if !is_window || !has_uri_list {
            offer.accept_mime_type(offer.serial, None);
            offer.set_actions(DndAction::empty(), DndAction::empty());
            return;
        }

        offer.accept_mime_type(offer.serial, Some(TEXT_URI_LIST.to_owned()));
        offer.set_actions(DndAction::Copy, DndAction::Copy);

        let generation = self.dnd_state.next_generation;
        self.dnd_state.next_generation = generation.wrapping_add(1);

        if let Some(previous) = self.dnd_state.active.take() {
            if let Some(token) = previous.transfer {
                self.loop_handle.remove(token);
            }
            previous.offer.destroy();
        }

        self.dnd_state.active = Some(ActiveDrag {
            generation,
            offer,
            data_device: data_device.id(),
            window_id,
            position: (x, y),
            hovered: false,
            dropped: false,
            transfer: None,
        });

        self.dnd_report_position(window_id, (x, y));
        self.dnd_start_transfer(Phase::Hover);
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, data_device: &WlDataDevice) {
        let is_active_device = self
            .dnd_state
            .active
            .as_ref()
            .is_some_and(|drag| !drag.dropped && drag.data_device == data_device.id());

        if is_active_device {
            self.dnd_cancel_active();
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
        let Some(drag) = self.dnd_state.active.as_mut() else {
            return;
        };

        if drag.dropped || drag.data_device != data_device.id() {
            return;
        }

        drag.position = (x, y);
        let window_id = drag.window_id;
        self.dnd_report_position(window_id, (x, y));
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
    ) {
        let Some(drag) = self.dnd_state.active.as_mut() else {
            return;
        };

        if drag.dropped || drag.data_device != data_device.id() {
            return;
        }

        drag.dropped = true;

        if !self.dnd_start_transfer(Phase::Drop) {
            if let Some(drag) = self.dnd_state.active.take() {
                if drag.hovered {
                    self.events_sink
                        .push_window_event(WindowEvent::HoveredFileCancelled, drag.window_id);
                }
                drag.offer.destroy();
            }
        }
    }
}

impl DataOfferHandler for WinitState {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl DataSourceHandler for WinitState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: String,
        _: WritePipe,
    ) {
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

sctk::delegate_data_device!(WinitState);

/// Converts surface local logical coordinates into physical window coordinates.
fn drag_position(x: f64, y: f64, scale_factor: f64) -> PhysicalPosition<f64> {
    LogicalPosition::new(x, y).to_physical(scale_factor)
}

/// Extracts local file paths from a `text/uri-list` payload.
///
/// Comment lines, entries that are not `file` URIs, and URIs naming a remote host
/// are skipped.
fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    data.split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .filter_map(|uri| {
            let path = parse_file_uri(uri);
            if path.is_none() {
                warn!("Ignoring dropped URI {:?}", String::from_utf8_lossy(uri));
            }
            path
        })
        .collect()
}

/// Converts one `file` URI into a path.
fn parse_file_uri(uri: &[u8]) -> Option<PathBuf> {
    let scheme = uri.get(..5)?;
    if !scheme.eq_ignore_ascii_case(b"file:") {
        return None;
    }

    let rest = uri.get(5..)?;
    let path = match rest.strip_prefix(b"//") {
        Some(authority_and_path) => {
            let slash = authority_and_path.iter().position(|&byte| byte == b'/')?;
            let (host, path) = authority_and_path.split_at(slash);
            if !host.is_empty() && !host.eq_ignore_ascii_case(b"localhost") {
                return None;
            }
            path
        },
        None if rest.starts_with(b"/") => rest,
        None => return None,
    };

    let decoded: Vec<u8> = percent_decode(path).collect();
    if decoded.contains(&0) {
        return None;
    }

    Some(PathBuf::from(OsString::from_vec(decoded)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    fn is_near(position: PhysicalPosition<f64>, x: f64, y: f64) -> bool {
        (position.x - x).abs() < 1e-9 && (position.y - y).abs() < 1e-9
    }

    #[test]
    fn drag_position_at_integer_scale() {
        assert!(is_near(drag_position(10.0, 20.5, 2.0), 20.0, 41.0));
    }

    #[test]
    fn drag_position_at_fractional_scale() {
        assert!(is_near(drag_position(100.0, 40.0, 1.25), 125.0, 50.0));
        assert!(is_near(drag_position(8.0, 2.0, 1.5), 12.0, 3.0));
        assert!(is_near(drag_position(10.0, 10.0, 1.2), 12.0, 12.0));
    }

    #[test]
    fn drag_position_at_origin_and_unit_scale() {
        assert!(is_near(drag_position(0.0, 0.0, 1.75), 0.0, 0.0));
        assert!(is_near(drag_position(33.25, 7.5, 1.0), 33.25, 7.5));
    }

    #[test]
    fn single_file() {
        assert_eq!(parse_uri_list(b"file:///home/user/a.txt\r\n"), vec![PathBuf::from(
            "/home/user/a.txt"
        )]);
    }

    #[test]
    fn multiple_files_and_line_endings() {
        let paths = parse_uri_list(b"file:///a\r\nfile:///b\nfile:///c");
        assert_eq!(paths, vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")]);
    }

    #[test]
    fn percent_decoding() {
        let paths = parse_uri_list(b"file:///home/user/My%20File%23%25.txt\r\n");
        assert_eq!(paths, vec![PathBuf::from("/home/user/My File#%.txt")]);
    }

    #[test]
    fn utf8_path() {
        let paths = parse_uri_list(b"file:///tonne/Gro%C3%9Fe.txt\r\n");
        assert_eq!(paths, vec![PathBuf::from("/tonne/Gro\u{df}e.txt")]);
    }

    #[test]
    fn non_utf8_path() {
        let paths = parse_uri_list(b"file:///data/%FF%FEname\r\n");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].as_os_str().as_bytes(), b"/data/\xff\xfename");
    }

    #[test]
    fn localhost_and_short_form() {
        let paths = parse_uri_list(b"file://localhost/x\r\nFILE:///y\r\nfile:/z\r\n");
        assert_eq!(paths, vec![PathBuf::from("/x"), PathBuf::from("/y"), PathBuf::from("/z")]);
    }

    #[test]
    fn skips_comments_remote_hosts_and_other_schemes() {
        let data = b"# comment\r\nfile://otherhost/x\r\nhttps://example.com/a\r\nfile:///ok\r\n";
        assert_eq!(parse_uri_list(data), vec![PathBuf::from("/ok")]);
    }

    #[test]
    fn rejects_malformed_entries() {
        let data = b"file:\r\nfile://\r\nfile://host\r\nfile:relative\r\nfile:///nul%00byte\r\nfi";
        assert!(parse_uri_list(data).is_empty());
    }

    #[test]
    fn empty_and_blank_input() {
        assert!(parse_uri_list(b"").is_empty());
        assert!(parse_uri_list(b"\r\n\r\n\n").is_empty());
    }

    #[test]
    fn invalid_percent_sequences_stay_literal() {
        let paths = parse_uri_list(b"file:///a%2/b%zz%\r\n");
        assert_eq!(paths, vec![PathBuf::from("/a%2/b%zz%")]);
    }
}
