# Why this fork exists

Branched from `v0.30.13`. It carries three changes on top of the release. The
drag and drop parts become obsolete once a stable upstream 0.31 is
adopted. Delete the whole fork at that point, provided the Android hover change
is upstream by then as well.

## 1. Android hover reaches the view hierarchy

One change in `src/platform_impl/android/mod.rs`.

Hover `MotionEvent` actions fell into the catch-all arm, which produces no winit
event and leaves the input status at `Handled`. android-activity then finishes
the event and `ViewRootImpl` consumes it before any `View` sees it, so no
`TYPE_VIEW_HOVER_ENTER` ever leaves the window. Android exposes no positional
hit test on `AccessibilityNodeProvider`, so a pointer can only reach a virtual
accessibility node through a hover event that arrives at the host view.

`HoverEnter`, `HoverMove` and `HoverExit` now leave the input status at
`Unhandled`. Slint's Android backend does the same for enter and exit.

## 2. File drag and drop on Wayland

winit 0.30 has no drag and drop on Wayland. Upstream added it for 0.31
(rust-windowing/winit PR #4571, `winit-wayland/src/dnd.rs`), with a new event
API and on smithay-client-toolkit 0.21. This fork ports the receiving side to
0.30 and its smithay-client-toolkit 0.19.2, and reports drops through the
existing 0.30 events, so applications written against 0.30 work unchanged.

Files:

- `src/platform_impl/linux/wayland/dnd.rs`: new. Data device, data offer and
  data source handlers, the `text/uri-list` transfer, and URI parsing.
- `src/platform_impl/linux/wayland/state.rs`: binds `wl_data_device_manager`
  when the compositor offers it and holds the drag state.
- `src/platform_impl/linux/wayland/seat/mod.rs`: creates one `wl_data_device`
  per seat.
- `src/platform_impl/linux/wayland/mod.rs`: registers the module.
- `Cargo.toml`: the `wayland` feature enables the already present
  `percent-encoding` dependency.

Behaviour, matching the X11 backend of 0.30:

- An offer is accepted on enter only when it advertises `text/uri-list` and the
  surface is one of our windows. It is accepted with that MIME type and the
  `copy` action. Anything else is rejected, so the source sees no target.
- On enter the list is read once and `HoveredFile` is emitted for every file.
- Leaving the window or a cancelled drag emits `HoveredFileCancelled`.
- On drop the list is read again, `DroppedFile` is emitted for every file, and
  the offer is finished and destroyed. When no file could be read from a drop,
  `HoveredFileCancelled` is emitted instead if hover events were sent before.
- Pointer motion during a drag is reported as `CursorMoved`, see section 3.
- Only `file:` URIs without a host or with `localhost` are reported. Paths are
  percent decoded into raw bytes, so non UTF-8 names survive. Comments, other
  schemes, remote hosts and paths containing NUL are skipped with a warning.
  Paths are not canonicalized.
- Transfers are read without blocking through the event loop and capped at
  4 MiB. Malformed or oversized data is logged and ignored.

Not ported: outgoing drags, selection (clipboard) offers and action
negotiation, since 0.30 has no API for them.

## 3. The pointer is reported while a file is dragged

In winit 0.30 no backend reports where a file is dragged or dropped. The drag
source owns the pointer during the drag, so the target window gets no regular
pointer motion: XDND and Wayland deliver positions only through drag messages,
Windows OLE through `IDropTarget`, and AppKit through `NSDraggingDestination`.
Upstream 0.31 carries the position in new `DragEntered` and `DragPosition` events.
This fork keeps the 0.30 API and sends `WindowEvent::CursorMoved` instead.

On every desktop backend, for a drag that is accepted as a file drag:

- `CursorMoved` is sent when the drag enters the window, on every movement over
  it, and once more directly before the first `DroppedFile` of a drop.
- The position is physical and relative to the window, like any other
  `CursorMoved`. The device ID is the one the backend uses for its pointer.
- Drags that are rejected (no files) produce no `CursorMoved`.
- No `CursorEntered` or `CursorLeft` is synthesized. The platforms send their
  own pointer enter and leave around a drag, and synthetic ones would pair up
  with those wrongly, for example a second enter after the drop.

Per backend:

- Wayland, `src/platform_impl/linux/wayland/dnd.rs`: from `wl_data_device`
  enter and motion, converted from surface local logical coordinates with the
  window scale factor, which includes fractional scale. The last position is
  repeated before `DroppedFile` once the drop data is read.
- X11, `src/platform_impl/linux/x11/dnd.rs` and `event_processor.rs`: from each
  accepted `XdndPosition`. The root coordinates are translated into the window
  and the device is the client pointer (`XIGetClientPointer`). `XdndDrop`
  carries no position, so the last one is repeated before `DroppedFile`.
- Windows, `src/platform_impl/windows/drop_handler.rs`: from the screen point
  of `DragEnter`, `DragOver` and `Drop`, through `ScreenToClient`. On enter and
  drop it is sent before the first file event.
- macOS, `src/platform_impl/macos/window_delegate.rs`: from `draggingLocation`
  in `draggingEntered:`, the newly implemented `draggingUpdated:` and
  `performDragOperation:`, converted like regular mouse motion.
  `draggingUpdated:` returns the same operation as `draggingEntered:`.
