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
- Outgoing drags (`start_drag`) are still not implemented on X11.

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
receiving.

## 3. No repeated drag positions on Windows

`winit-win32/src/dnd.rs`: OLE calls `IDropTarget::DragOver` periodically while
the pointer rests. `DragPosition` is sent only when the client point or the
proposed action differs from the last one sent for the transfer.
