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
  last accepted action (older targets), a move turned into copy as section 6
  describes. A failed finish, a release over no
  accepting target, Escape, loss of the selection and unmapping or destroying
  the source window emit `OutgoingDragCanceled`.
- A target that does not answer `XdndPosition` within 2 seconds is left and not
  entered again until the pointer leaves it. A drop without `XdndFinished`
  within 10 seconds is canceled. The event loop wakes up for these deadlines.
- `SelectionRequest` is answered for `TARGETS`, `TIMESTAMP`, `MULTIPLE`,
  `DELETE` (section 6) and every offered type. URIs are sent as `text/uri-list` with CRLF, `STRING` as
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

## 5. Performed action of an outgoing drag on Windows

`winit-win32/src/dnd.rs`, `event_loop/runner.rs`: the shell moves files with an
optimized move and returns `DROPEFFECT_NONE` from `DoDragDrop`. The source data
object keeps what the target stores with `SetData`, and `OutgoingDragDropped`
reports, in this order, the nonzero `Logical Performed DropEffect`, the effect
returned by `DoDragDrop` when it is not `DROPEFFECT_NONE`, else the stored
`Performed DropEffect`.

## 6. Move on X11 follows `DELETE`

XDND implements `XdndActionMove` by converting the data and then the target
`DELETE` before `XdndFinished` (`atoms.rs`: `DELETE`, `NULL`).

Source (`xdnd_source.rs`, `drag_source.rs`):

- `DELETE` is answered between `XdndDrop` and `XdndFinished` with an empty
  property of type `NULL`, and refused before the drop and after the finish.
- `OutgoingDragDropped` reports `Move` only when the target converted `DELETE`
  before `XdndFinished` and the finish names move (version 5) or move was the
  last accepted action (version 4). A finished move without `DELETE` is
  reported as `Copy`, since the data was not deleted at the target's request.

Target (`dnd.rs`, `event_processor.rs`, `drag_source.rs`):

- After a drop with the accepted action move and at least one completed data
  transfer, the target converts `DELETE` and sends `XdndFinished` when the
  answer arrives, or after 2 seconds without one (`drag_deadline` and
  `drag_tick`). A drop without received data finishes at once.
- A `SelectionNotify` with property `None` is handled before the property
  check: it pops the refused fetch, or answers `DELETE`, also when the target
  field is `None`. A refused or unreadable fetch moves on to the next fetch or
  to the end of the drop instead of leaving the drop unfinished.
- `XdndStatus` and `XdndFinished` without an accepted action carry 0 in the
  action field, not the atom named `None`.

Observed peers (Xvfb, `xtrace`):

- GTK 3 as target converts `DELETE` after a move (`gtk_drag_finish` with
  `del`), so a drag to GTK reports `Move`; with Control it reports `Copy`.
  Mousepad finishes a dropped file with move but never converts `DELETE`, so
  that drop reports `Copy`.
- Qt 5 as target finishes with move and never converts `DELETE`, so a drag to
  Qt reports `Copy`.
- GTK 3 as source answers the `DELETE` of a winit target and emits
  `drag-data-delete`. Qt 5 as source refuses `DELETE` with target `None` and
  deletes on the finished move by itself.

## 7. Dropped offers on Wayland

`winit-wayland/src/dnd.rs`, `event_loop/mod.rs`: a dropped offer is kept until
it is finished or destroyed, so the application can fetch data and answer
`ask` after `DragDropped`. Upstream finished or destroyed it during the drop.

- The `leave` that follows a drop neither emits `DragLeft` nor destroys the
  offer. `accept` is not sent for fetches after the drop.
- Transfers started on the offer are counted until their pipe is read to the
  end.
- After the event loop delivered `DragDropped` and the application handled the
  events of that iteration, the offer is finished and destroyed when its action
  is copy or move, version 3 or newer and no transfer is open. Without a final
  action (or before version 3) it is only destroyed, which cancels the source.
- For a drop with the action `ask`, `set_valid_dnd_actions` after the drop sends
  `set_actions` with the first copy or move of the list that the source offers,
  as both the accepted and the preferred action, and the offer finishes as
  above. A list without such an action destroys the offer.
- A dropped offer that has not ended 10 seconds after the drop is destroyed.

## 8. Trash on macOS

`winit-appkit/src/dnd.rs`: the source mask of an outgoing drag that allows move
also allows `NSDragOperationDelete`, and an ended drag with the operation
`Delete` (a drop on the Trash) reports `Move`.

## 9. Drag and drop on Android

Upstream returns `NotSupported` from every data transfer method on Android. The
fork adds `winit-android/src/dnd.rs` (state without a virtual machine, unit
tested), `drag_and_drop.rs` (JNI), the Java listener
`java/org/rustwindowing/winit/DragAndDrop.java` compiled by `build-dex.sh` into
`dnd.dex`, hooks in `event_loop.rs`, and `jni` as a direct dependency (the
version `android-activity` already uses).

- `dnd.dex` is embedded and loaded with `InMemoryDexClassLoader` without a
  parent. Its native methods are registered on load. On every
  `MainEvent::Start` the listener is set as `View.OnDragListener` of the decor
  view, on the UI thread. Its callbacks fill a queue and wake the event loop,
  which delivers the events to the window after the input events.
- `ACTION_DRAG_ENTERED` emits `DragEntered` without a position (Android gives
  none), `ACTION_DRAG_LOCATION` emits `DragPosition`, `ACTION_DRAG_EXITED` and
  an `ACTION_DRAG_ENDED` without a drop emit `DragLeft`. Positions waiting in
  the queue are coalesced.
- Android knows no drag actions. A winit source writes its copy and move codes
  into the extras of the `ClipDescription`; a foreign source offers copy. The
  proposed action is the first of `set_valid_dnd_actions` that the source
  offers. `ACTION_DROP` without one returns false and emits `DragLeft`.
  Otherwise it emits a last `DragPosition` and `DragDropped` with the action.
- Types are the MIME types of the description (at most 64). `text/plain` and
  `text/html` carry their hints; every other type stands for items with a URI
  and adds `text/uri-list`. At the drop the text, HTML and URI of every item are
  read (at most 256 items and 32 MiB). Fetches made before the drop are answered
  after it, and later fetches are answered at once until the next drag enters.
- A drop with `content:` URIs from another application calls
  `requestDragAndDropPermissions`. The permissions are kept until the next such
  drop.
- `start_drag` hands text, HTML and the URI list to `startDragAndDrop` with
  `DRAG_FLAG_GLOBAL | DRAG_FLAG_GLOBAL_URI_READ` on the UI thread. Bytes, image
  and audio types are not sent, and `file:` URIs are refused, since Android
  throws `FileUriExposedException` for them. Only copy and move are offered. An
  `RgbaIcon` of at most 1024 by 1024 pixels is the drag shadow, touched at the
  negated offset.
- A winit target answers its action through a `Binder` that travels in the
  intent of the first item. `ACTION_DRAG_ENDED` with a result emits
  `OutgoingDragDropped`: `Move` only when a winit target answered move, else
  `Copy`. Without a result, or when the drag could not be started, it emits
  `OutgoingDragCanceled`. Nothing is deleted on move; the action is a report.

Observed in Waydroid (Android 13, two test applications with different package
names in freeform windows, `input draganddrop`): a move inside one application,
copy and move from one application into the other with the data received, and a
drop on the launcher reported as `OutgoingDragCanceled`.

## 10. Drag and drop on iOS

Upstream returns `NotSupported` from every data transfer method on iOS. The fork
adds `winit-uikit/src/dnd.rs`, delegates on `WinitView` in `view.rs`, the
methods in `event_loop.rs` and the drag state in `app_state.rs`.

Receiving (`UIDropInteraction` on the view of each window):

- `sessionDidEnter` emits `DragEntered` with the position, `sessionDidUpdate`
  emits `DragPosition`, `sessionDidExit` emits `DragLeft`, `performDrop` emits a
  last `DragPosition` and `DragDropped`. A session that ends without either
  emits `DragLeft`. Positions are `locationInView` times `contentScaleFactor`.
- The proposal is the first action passed to `set_valid_dnd_actions` that the
  session allows: copy always, move only for a drag of the same application
  whose session allows move. It is answered as `UIDropOperation` copy or move,
  an empty list cancels and a list without an allowed action forbids. The same
  action is reported as `proposed_action`.
- `data_transfer` lists the registered type identifiers of all items (at most
  1024 items and 256 types). Plain text, HTML, RTF, URL and file URL, PNG,
  JPEG, TIFF, GIF, HEIC, MP3, WAV, AIFF and M4A carry a `TypeHint`;
  `public.image` and `public.audio` map to image and audio without extension.
- `fetch_data_transfer` loads a data representation of at most 64 MiB and
  delivers `DataTransferReceived` on the main queue. Data of another
  application is released only by the drop, so a fetch before the drop starts
  when the drop happens. `UriList` collects one URI per item from `public.url`
  or `public.file-url`, or copies the file representation of an item without
  one (at most 1 GiB) into `winit-dnd` in the temporary directory and reports
  its file URI.

Sending (`UIDragInteraction` on the same view):

- UIKit starts a drag only with its own lift gesture. `start_drag` arms the
  data while a touch of the source window is down and returns its ID; the next
  lift in that window takes it. The last touch ending without a lift, or a new
  `start_drag`, emits `OutgoingDragCanceled` for the armed drag. Without armed
  data the lift yields no items and no drag begins.
- The first item registers every offered type, loaded from the
  `DataTransferSend` on request, and the first URI; every further URI is an
  item of its own (`public.url`, and `public.file-url` for `file:` URIs). Move
  is allowed when the actions contain it; the session is not restricted to the
  application.
- An `RgbaIcon` is the lift preview, placed at the icon offset; without one the
  preview is a 1 point view.
- `didEndWithOperation` copy or move emits `OutgoingDragDropped` with that
  action, cancel and forbidden emit `OutgoingDragCanceled`. Nothing is deleted
  at the target's request; a move is a report only.

Checked with clippy for `aarch64-apple-ios` only; never run.

## 11. Drag and drop on the web

`winit-web/src/dnd.rs`, `web_sys/dnd.rs`, `web_sys/canvas.rs`,
`web_sys/pointer.rs`, `event_loop/window_target.rs`, web-sys features for drag
events and files. Upstream returns `NotSupported` from all four data transfer
methods on the web.

- The canvas listens to `dragenter`, `dragover`, `dragleave` and `drop`.
  `DragEntered` carries the position, `DragPosition` is sent only when the
  position or the action changed. A `dragleave` whose related target lies
  inside the canvas is ignored.
- The action is the requested one (Shift for move, Control for copy, both for
  link) when the application and `effectAllowed` of the source allow it, else
  the first action passed to `set_valid_dnd_actions` that the source allows. It
  is answered as `dropEffect`. Every `dragover` is canceled. A rejected drag
  answers `none`, so the browser fires no `drop`, does not open a dropped file,
  and the window receives `DragLeft`.
- Types come from `DataTransfer.types` and the file items: `text/plain`,
  `text/html`, `text/uri-list` and `text/rtf` map to their hints, files of type
  `image/*` and `audio/*` to `Image` and `Audio`, everything else has no hint.
  `WebTransferType` exposes the MIME type and, after the drop, the file name.
- Strings are readable only during `drop`, so every string type is read then
  (at most 16 MiB each) and every file is read with `arrayBuffer()` (at most
  256 MiB). Fetches made before the drop are answered after it. At most 64
  types and 256 waiting fetches are kept. The data stays available until the
  next drag enters.
- `start_drag` works only while a button is held on the source canvas, since
  browsers start a drag only from a gesture. It makes the canvas `draggable`.
  `dragstart` fills the text types (`setData`), `effectAllowed` from `actions`
  and the drag image from an `RgbaIcon`. `dragend` emits `OutgoingDragDropped`
  with the `dropEffect`, or `OutgoingDragCanceled` for `none`. A release
  without `dragstart` cancels the drag.
- `pointerdown` calls `preventDefault` only after the handler ran and only when
  the handler did not prepare a drag, since a canceled `pointerdown` suppresses
  the drag.
- Image and audio data are not sent: Chrome drops files added in `dragstart`
  and then reports the file name as `text/plain`.
- Move deletes nothing. `OutgoingDragDropped` with `Move` means the target
  answered `dropEffect` move. During a drag the browser sends no pointer
  events, and the release of the button does not reach the application.

## 12. Mouse and stylus on Android

`winit-android/src/pointer.rs` (unit tested), `event_loop.rs`. Upstream skips
every pointer whose tool is `ToolType::Mouse` and reports styluses as
`Unknown`.

- Fingers report `Touch` as before. Unknown tools report `Unknown`.
- A mouse reports `PointerKind::Mouse`, a stylus `TabletTool(Pen)` and an
  eraser `TabletTool(Eraser)`, with `primary` set. Hover enter and move emit
  `PointerEntered` and `PointerMoved`. Android ends the hover right before a
  press and starts it again after the release, so a hover exit becomes
  `PointerLeft` only when the input batch ends without a further event of the
  same device. A change of the tool on one device leaves with the old kind and
  enters with the new one. Hover events stay `Unhandled` (section 1).
- Mouse buttons follow the button state: primary, secondary, tertiary, back and
  forward become `Left`, `Right`, `Middle`, `Back` and `Forward`. A press
  without a button state, as injected input sends it, is `Left`.
- A stylus contact is `TabletToolButton::Contact`, the stylus buttons are
  `Barrel` and `Other(1)`. Tool data carries the pressure as force and the angle
  from `AXIS_TILT` and `AXIS_ORIENTATION` (altitude π/2 minus the tilt,
  azimuth the orientation turned to 3 o'clock).
- `ACTION_SCROLL` emits `MouseWheel` with `LineDelta(-AXIS_HSCROLL,
  AXIS_VSCROLL)`. A cancel releases held buttons and leaves.

Observed in Waydroid: `input mouse tap` and `input mouse swipe` report a mouse
with `Left`, `input stylus swipe` a pen with its contact. Hover and scroll could
not be injected there: the shell `input` of Android 13 has no hover action or
scroll command, and the shell user cannot open `/dev/uhid`.
