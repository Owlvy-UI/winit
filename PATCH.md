# Why this fork exists

Branched from upstream `v0.31.0-beta.3` (commit `7d20408`). Every deviation from
that release is listed here. Delete the fork once these changes are upstream.

## 1. Android hover reaches the view hierarchy

`winit-android/src/event_loop.rs`, `handle_input_event`.

`HoverEnter`, `HoverMove` and `HoverExit` leave the input status at
`Unhandled`. Upstream lets them fall into the catch-all arm, which reports them
as `Handled`, so `ViewRootImpl` consumes them before any `View` sees them and
accessibility never gets a hover over a virtual node.

## 2. Copy and move negotiation

X11 (`winit-x11/src/dnd.rs`, `event_processor.rs`, `event_loop.rs`,
`atoms.rs`):

- The action requested in `XdndPosition` (`data.l[4]`, copy before version 2)
  is accepted when it is in the list passed to `set_valid_dnd_actions`.
  Otherwise copy is accepted if allowed, else the drag is rejected. Only
  `XdndActionCopy` and `XdndActionMove` are supported.
- The accepted action is sent in `XdndStatus` and `XdndFinished` and reported
  as `proposed_action` of `DragPosition` and `DragDropped`. Upstream always
  answered copy.
- Each `XdndEnter` starts a new drag state. Upstream kept the state of the
  first drag for every later one.
- A drop without an accepted action emits `DragLeft` instead of `DragDropped`.
- Outgoing drags (`start_drag`) are added in section 4.

Wayland (`winit-wayland/src/dnd.rs`):

- An offer that leaves without a drop is destroyed, not finished. Upstream
  called `wl_data_offer.finish` on leave.
- A dropped offer is finished only when the selected action is exactly copy or
  move and the offer is version 3 or newer.
- `OutgoingDragDropped` is emitted on `wl_data_source.dnd_finished` with the
  final action, not on `dnd_drop_performed`. `cancelled` emits
  `OutgoingDragCanceled` and clears the drag.

Windows (`winit-win32/src/dnd.rs`, `event_loop.rs`, `event_loop/runner.rs`):

- `proposed_action` of `DragPosition` is the action of the effect picked with
  the held modifier keys. `DragDropped` reports the effect returned to the
  source.
- A drop on a window of the same process accepts the actions passed to
  `start_drag`, since the application handler cannot run during `DoDragDrop`.

macOS (`winit-appkit/src/dnd.rs`, `window_delegate.rs`):

- A source mask reduced to `Generic` (Command held) accepts move and returns
  `Generic` to AppKit.

`winit-core/src/event_loop/mod.rs`: the `DndAction::Move` docs list X11 for
receiving (for sending as well since section 4).

## 3. No repeated drag positions on Windows

`winit-win32/src/dnd.rs`: OLE calls `IDropTarget::DragOver` periodically while
the pointer rests. `DragPosition` is sent only when the client point or the
proposed action differs from the last one sent for the transfer.

## 4. Outgoing drags on X11

Upstream returns `NotSupported` from `start_drag` on X11. The fork adds an XDND
source (`winit-x11/src/xdnd_source.rs`, `drag_source.rs`, hooks in
`event_loop.rs` and `event_processor.rs`, atoms in `atoms.rs`, the `shape`
feature of `x11rb`):

- `xdnd_source.rs` holds the protocol logic without a server connection: the
  state machine, message packing, version and proxy checks, action choice,
  timeouts, `INCR` pieces, text and URI list encoding, icon pixels. It is unit
  tested.
- `start_drag` takes `XdndSelection`, sets `XdndTypeList`, grabs the client
  pointer and its paired master keyboard on the source window with
  `XIGrabDevice` and shows the `no-drop`, `copy` or `move` cursor for the
  current answer of the target (a new `XIGrabDevice` with the other cursor).
  A core `GrabPointer` fails with `AlreadyGrabbed` because the button press
  that starts the drag holds an implicit XI2 grab. Every end of the drag calls
  `XIUngrabDevice` for both devices.
- XI2 motion, button and key events on the grab window go to the drag
  (`EventProcessor::drag_grab_event`) and never reach the application as
  `PointerMoved`, `PointerButton` or key input. XI2 enter, leave and focus
  events of the grab (mode `Grab`, and `Ungrab` up to one second after the
  drag) are dropped as well. Position and modifiers come from `QueryPointer`.
- Targets are found by descending from the root window to the first window
  with `XdndAware`, honouring a valid `XdndProxy`. Versions 3 to 5 are spoken.
- `XdndPosition` waits for the previous `XdndStatus` and is skipped inside the
  rectangle of a status without bit 1. A release while a status is due waits
  for it, a release before any status leaves.
- Only `XdndActionCopy` and `XdndActionMove` are requested: the first of the
  allowed actions, Shift for move, Control for copy. A status naming an action
  outside the allowed list counts as a rejection.
- `XdndFinished` emits `OutgoingDragDropped` with its action (version 5) or the
  last accepted action (older targets). A failed finish, a release over no
  accepting target, Escape, loss of the selection and unmapping or destroying
  the source window emit `OutgoingDragCanceled`.
- A target that does not answer `XdndPosition` within 2 seconds is left and not
  entered again until the pointer leaves it. A drop without `XdndFinished`
  within 10 seconds is canceled. The event loop wakes up for these deadlines.
- `SelectionRequest` is answered for `TARGETS`, `TIMESTAMP`, `MULTIPLE` and
  every offered type. URIs are sent as `text/uri-list` with CRLF, `STRING` as
  ISO-8859-1 (refused when the text does not fit), other text as UTF-8. Data
  larger than one request goes by `INCR` to foreign windows, at most 16
  transfers at once with a 10 second stall timeout.
- An `RgbaIcon` is shown in an override-redirect window that follows the
  pointer at the icon offset, with an empty input shape and a bounding shape of
  its visible pixels, on a 32 bit visual when there is one.
- `SelectionType` gets its hint table as `hint_table`, ordered with the UTF-8
  targets first, and `next_transfer_id` hands out IDs for both directions.
  `XConnection::cached_cursor` returns a cursor from the existing cache.

`winit-core`: the `start_drag` docs describe X11, and the `DragPosition` docs no
longer claim that X11 reports the action only at the end.
