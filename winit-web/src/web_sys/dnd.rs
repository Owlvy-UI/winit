//! Drag and drop through the HTML drag events of the canvas.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use dpi::PhysicalPosition;
use js_sys::{Uint8Array, Uint8ClampedArray};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    CanvasRenderingContext2d, DataTransfer, Document, DragEvent, File, HtmlCanvasElement,
    HtmlElement, ImageData, Node, PointerEvent,
};
use winit_core::data_transfer::{
    DataTransfer as CoreDataTransfer, DataTransferId, DataTransferSend, SendData, TransferType,
};
use winit_core::error::{NotSupportedError, RequestError};
use winit_core::event::WindowEvent;
use winit_core::event_loop::{AsyncRequestSerial, DndAction, DragIcon};
use winit_core::icon::RgbaIcon;
use winit_core::window::WindowId;

use super::super::dnd::{
    self, Allowed, MAX_FILE_BYTES, MAX_PENDING_FETCHES, MAX_TYPES, Payload, WebData,
    WebDataTransfer, WebTransferType,
};
use super::super::event_loop::runner::{self, Event};
use super::event;
use super::event_handle::EventListenerHandle;

/// The state of one dropped or offered type.
#[derive(Debug)]
enum Slot {
    /// Not dropped yet.
    Waiting,
    /// A file being read.
    Loading,
    /// Read and ready to hand out.
    Ready(Arc<WebData>),
    /// Not readable, or over its limit.
    Unavailable,
}

/// A drag over one of the canvases.
#[derive(Debug)]
struct Incoming {
    id: DataTransferId,
    window_id: WindowId,
    types: Vec<WebTransferType>,
    slots: Vec<Slot>,
    allowed: Allowed,
    valid: Vec<DndAction>,
    action: Option<DndAction>,
    position: Option<PhysicalPosition<f64>>,
    dropped: bool,
    pending: Vec<(AsyncRequestSerial, usize)>,
}

/// A drag started with `start_drag`.
struct Outgoing {
    id: DataTransferId,
    window_id: WindowId,
    canvas: HtmlCanvasElement,
    data: Box<dyn DataTransferSend>,
    actions: Vec<DndAction>,
    icon: Option<DragIcon>,
    started: bool,
}

#[derive(Default)]
struct State {
    next_id: i64,
    incoming: Option<Incoming>,
    outgoing: Option<Outgoing>,
    pressed: Option<(WindowId, HtmlCanvasElement)>,
}

impl State {
    fn new_id(&mut self) -> DataTransferId {
        self.next_id = self.next_id.wrapping_add(1);
        DataTransferId::from_raw(self.next_id)
    }

    fn incoming(&mut self, id: DataTransferId) -> Option<&mut Incoming> {
        self.incoming.as_mut().filter(|incoming| incoming.id == id)
    }
}

/// Drag and drop state shared by the event loop and the canvases.
#[derive(Clone, Default)]
pub(crate) struct DragAndDrop(Rc<RefCell<State>>);

impl std::fmt::Debug for DragAndDrop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DragAndDrop").finish_non_exhaustive()
    }
}

/// The error returned for a data transfer that does not exist.
fn unknown_transfer() -> RequestError {
    RequestError::NotSupported(NotSupportedError::new("the data transfer does not exist"))
}

impl DragAndDrop {
    /// Requests the data of a type of an incoming drag.
    pub(crate) fn fetch(
        &self,
        runner: &runner::Shared,
        id: DataTransferId,
        type_: &dyn TransferType,
    ) -> Result<AsyncRequestSerial, RequestError> {
        let mut state = self.0.borrow_mut();
        let incoming = state.incoming(id).ok_or_else(unknown_transfer)?;
        let index = dnd::find_type(&incoming.types, type_).ok_or_else(|| {
            RequestError::NotSupported(NotSupportedError::new("the drag does not offer this type"))
        })?;
        let serial = AsyncRequestSerial::get();

        match incoming.slots.get(index) {
            Some(Slot::Ready(value)) => {
                let event = Event::WindowEvent {
                    window_id: incoming.window_id,
                    event: WindowEvent::DataTransferReceived { id, serial, value: value.clone() },
                };
                drop(state);
                runner.send_event(event);
            },
            Some(Slot::Waiting | Slot::Loading) => {
                if incoming.pending.len() >= MAX_PENDING_FETCHES {
                    return Err(RequestError::Ignored);
                }
                incoming.pending.push((serial, index));
            },
            Some(Slot::Unavailable) | None => return Err(RequestError::Ignored),
        }

        Ok(serial)
    }

    /// The types of an incoming drag.
    pub(crate) fn data_transfer(
        &self,
        id: DataTransferId,
    ) -> Result<Box<dyn CoreDataTransfer>, RequestError> {
        let mut state = self.0.borrow_mut();
        let incoming = state.incoming(id).ok_or_else(unknown_transfer)?;

        Ok(Box::new(WebDataTransfer { types: incoming.types.clone() }))
    }

    /// Sets the actions the application accepts for an incoming drag.
    pub(crate) fn set_valid_actions(
        &self,
        id: DataTransferId,
        actions: &[DndAction],
    ) -> Result<(), RequestError> {
        let mut state = self.0.borrow_mut();
        let incoming = state.incoming(id).ok_or_else(unknown_transfer)?;
        incoming.valid = actions.to_vec();

        Ok(())
    }

    /// Prepares an outgoing drag that the browser starts when the held button moves.
    pub(crate) fn start(
        &self,
        runner: &runner::Shared,
        source: WindowId,
        data: Box<dyn DataTransferSend>,
        actions: &[DndAction],
        icon: Option<DragIcon>,
    ) -> Result<DataTransferId, RequestError> {
        if dnd::effect_allowed(actions) == "none" {
            return Err(RequestError::NotSupported(NotSupportedError::new(
                "a drag on the web needs copy, move or link",
            )));
        }

        let mut state = self.0.borrow_mut();
        let Some(canvas) = state
            .pressed
            .as_ref()
            .filter(|(window_id, _)| *window_id == source)
            .map(|(_, canvas)| canvas.clone())
        else {
            return Err(RequestError::NotSupported(NotSupportedError::new(
                "a drag on the web starts only while a button is held on the canvas",
            )));
        };

        let replaced = state.outgoing.take();
        let id = state.new_id();
        canvas.set_draggable(true);
        state.outgoing = Some(Outgoing {
            id,
            window_id: source,
            canvas,
            data,
            actions: actions.to_vec(),
            icon,
            started: false,
        });
        drop(state);

        if let Some(replaced) = replaced {
            runner.send_event(Event::WindowEvent {
                window_id: replaced.window_id,
                event: WindowEvent::OutgoingDragCanceled { id: replaced.id },
            });
        }

        Ok(id)
    }
}

/// The listeners of one canvas.
pub(crate) struct DragHandler {
    _drag: Vec<EventListenerHandle<dyn FnMut(DragEvent)>>,
    _pointer: Vec<EventListenerHandle<dyn FnMut(PointerEvent)>>,
}

/// Everything a listener needs.
#[derive(Clone)]
struct Context {
    dnd: DragAndDrop,
    runner: runner::Shared,
    window_id: WindowId,
    canvas: HtmlCanvasElement,
    window: web_sys::Window,
    document: Document,
}

impl DragHandler {
    /// Adds the drag listeners to a canvas.
    pub(crate) fn new(
        dnd: &DragAndDrop,
        runner: runner::Shared,
        window_id: WindowId,
        canvas: &HtmlCanvasElement,
        window: &web_sys::Window,
        document: &Document,
    ) -> Self {
        let context = Context {
            dnd: dnd.clone(),
            runner,
            window_id,
            canvas: canvas.clone(),
            window: window.clone(),
            document: document.clone(),
        };
        let drag = |name: &'static str, handler: fn(&Context, &DragEvent)| {
            let context = context.clone();
            EventListenerHandle::new(
                canvas.clone(),
                name,
                Closure::new(move |event: DragEvent| handler(&context, &event))
                    as Closure<dyn FnMut(DragEvent)>,
            )
        };
        let pointer = |name: &'static str, handler: fn(&Context, &PointerEvent)| {
            let context = context.clone();
            EventListenerHandle::new(
                canvas.clone(),
                name,
                Closure::new(move |event: PointerEvent| handler(&context, &event))
                    as Closure<dyn FnMut(PointerEvent)>,
            )
        };

        Self {
            _drag: vec![
                drag("dragenter", on_drag_enter),
                drag("dragover", on_drag_over),
                drag("dragleave", on_drag_leave),
                drag("drop", on_drop),
                drag("dragstart", on_drag_start),
                drag("dragend", on_drag_end),
            ],
            _pointer: vec![
                pointer("pointerdown", on_pointer_down),
                pointer("pointerup", on_pointer_release),
                pointer("pointercancel", on_pointer_release),
            ],
        }
    }
}

/// The position of a drag event in the canvas.
fn position(context: &Context, event: &DragEvent) -> PhysicalPosition<f64> {
    event::pointer_position(event).to_physical(super::scale_factor(&context.window))
}

/// The types a drag offers, strings first, then files.
fn offered_types(data: &DataTransfer) -> Vec<WebTransferType> {
    let mut types: Vec<WebTransferType> = data
        .types()
        .iter()
        .filter_map(|name| name.as_string())
        .filter(|name| name != "Files")
        .map(|name| WebTransferType::string(&name))
        .take(MAX_TYPES)
        .collect();

    let items = data.items();
    let mut file_index = 0;
    for index in 0..items.length() {
        if types.len() >= MAX_TYPES {
            break;
        }
        let Some(item) = items.get(index) else { continue };
        if item.kind() == "file" {
            types.push(WebTransferType::file(file_index, &item.type_()));
            file_index += 1;
        }
    }

    types
}

/// Whether the pointer moved onto the canvas or something inside it.
fn stays_inside(context: &Context, event: &DragEvent) -> bool {
    event
        .related_target()
        .and_then(|target| target.dyn_into::<Node>().ok())
        .is_some_and(|node| context.canvas.contains(Some(&node)))
}

/// Starts a new incoming drag and tells the application.
fn enter(context: &Context, event: &DragEvent) {
    let Some(data) = event.data_transfer() else { return };
    let types = offered_types(&data);
    let position = position(context, event);

    let mut state = context.dnd.0.borrow_mut();
    let previous = state.incoming.take();
    let id = state.new_id();
    state.incoming = Some(Incoming {
        id,
        window_id: context.window_id,
        slots: types.iter().map(|_| Slot::Waiting).collect(),
        types,
        allowed: Allowed::from_effect_allowed(&data.effect_allowed()),
        valid: Vec::new(),
        action: None,
        position: Some(position),
        dropped: false,
        pending: Vec::new(),
    });
    drop(state);

    let left = previous.filter(|previous| !previous.dropped).map(|previous| Event::WindowEvent {
        window_id: previous.window_id,
        event: WindowEvent::DragLeft { id: previous.id },
    });
    let entered = Event::WindowEvent {
        window_id: context.window_id,
        event: WindowEvent::DragEntered { id, position: Some(position) },
    };
    context.runner.send_events(left.into_iter().chain([entered]));
}

/// Answers a drag position with the chosen action.
///
/// The event is always canceled, so a rejected drop does not fall back to the browser and open
/// the dropped file. A drop effect of `none` keeps the browser from firing `drop`.
fn answer(context: &Context, event: &DragEvent, report: bool) {
    let Some(data) = event.data_transfer() else { return };
    event.prevent_default();

    let position = position(context, event);
    let requested = dnd::requested_action(event.shift_key(), event.ctrl_key());

    let mut state = context.dnd.0.borrow_mut();
    let Some(incoming) = state.incoming.as_mut().filter(|incoming| !incoming.dropped) else {
        data.set_drop_effect("none");
        return;
    };
    let action = dnd::choose_action(&incoming.valid, incoming.allowed, requested);
    data.set_drop_effect(dnd::drop_effect(action));

    let changed = incoming.action != action || incoming.position != Some(position);
    incoming.action = action;
    incoming.position = Some(position);
    let id = incoming.id;
    drop(state);

    if report && changed {
        context.runner.send_event(Event::WindowEvent {
            window_id: context.window_id,
            event: WindowEvent::DragPosition { id, position, proposed_action: action },
        });
    }
}

fn on_drag_enter(context: &Context, event: &DragEvent) {
    let known = context
        .dnd
        .0
        .borrow()
        .incoming
        .as_ref()
        .is_some_and(|incoming| incoming.window_id == context.window_id && !incoming.dropped);

    if !known || !stays_inside(context, event) {
        enter(context, event);
    }

    answer(context, event, false);
}

fn on_drag_over(context: &Context, event: &DragEvent) {
    let known = context
        .dnd
        .0
        .borrow()
        .incoming
        .as_ref()
        .is_some_and(|incoming| incoming.window_id == context.window_id && !incoming.dropped);

    if known {
        answer(context, event, true);
    } else {
        enter(context, event);
        answer(context, event, false);
    }
}

fn on_drag_leave(context: &Context, event: &DragEvent) {
    if stays_inside(context, event) {
        return;
    }

    let mut state = context.dnd.0.borrow_mut();
    let Some(incoming) = state
        .incoming
        .take_if(|incoming| incoming.window_id == context.window_id && !incoming.dropped)
    else {
        return;
    };
    drop(state);

    context.runner.send_event(Event::WindowEvent {
        window_id: context.window_id,
        event: WindowEvent::DragLeft { id: incoming.id },
    });
}

/// Reads one dropped string under its limit.
fn read_string(data: &DataTransfer, type_: &WebTransferType) -> Slot {
    match data.get_data(type_.mime()) {
        Ok(text) if dnd::string_fits(&text) => {
            Slot::Ready(Arc::new(WebData::new(type_.clone(), Payload::Text(text))))
        },
        Ok(text) => {
            tracing::warn!("dropped {:?} of {} bytes is over the limit", type_.mime(), text.len());
            Slot::Unavailable
        },
        Err(error) => {
            tracing::warn!("dropped {:?} could not be read: {error:?}", type_.mime());
            Slot::Unavailable
        },
    }
}

fn on_drop(context: &Context, event: &DragEvent) {
    event.prevent_default();
    let Some(data) = event.data_transfer() else { return };
    let files = data.files();

    let mut state = context.dnd.0.borrow_mut();
    let Some(incoming) = state
        .incoming
        .as_mut()
        .filter(|incoming| incoming.window_id == context.window_id && !incoming.dropped)
    else {
        return;
    };

    let id = incoming.id;
    let action = incoming.action;
    if action.is_none() {
        state.incoming = None;
        drop(state);
        context.runner.send_event(Event::WindowEvent {
            window_id: context.window_id,
            event: WindowEvent::DragLeft { id },
        });
        return;
    }

    incoming.dropped = true;
    let mut reads = Vec::new();
    for (type_, slot) in incoming.types.iter_mut().zip(incoming.slots.iter_mut()) {
        let Some(index) = type_.file_index() else {
            *slot = read_string(&data, type_);
            continue;
        };
        let file = files
            .as_ref()
            .and_then(|files| u32::try_from(index).ok().and_then(|index| files.get(index)));
        let Some(file) = file else {
            *slot = Slot::Unavailable;
            continue;
        };
        type_.set_file_name(file.name());
        if dnd::within_limit(file.size(), MAX_FILE_BYTES) {
            *slot = Slot::Loading;
            reads.push((index, file));
        } else {
            tracing::warn!("dropped file of {} bytes is over the limit", file.size());
            *slot = Slot::Unavailable;
        }
    }

    let received = take_ready(incoming);
    drop(state);

    let dropped = Event::WindowEvent {
        window_id: context.window_id,
        event: WindowEvent::DragDropped { id, proposed_action: action },
    };
    context.runner.send_events([dropped].into_iter().chain(received));

    for (index, file) in reads {
        read_file(context.clone(), id, index, &file);
    }
}

/// Removes the fetches whose data is ready and returns their events.
fn take_ready(incoming: &mut Incoming) -> Vec<Event> {
    let (id, window_id) = (incoming.id, incoming.window_id);
    let slots = &incoming.slots;
    let mut events = Vec::new();

    incoming.pending.retain(|(serial, index)| match slots.get(*index) {
        Some(Slot::Ready(value)) => {
            events.push(Event::WindowEvent {
                window_id,
                event: WindowEvent::DataTransferReceived {
                    id,
                    serial: *serial,
                    value: value.clone(),
                },
            });
            false
        },
        Some(Slot::Waiting | Slot::Loading) => true,
        Some(Slot::Unavailable) | None => false,
    });

    events
}

/// Reads a dropped file and answers the fetches waiting for it.
fn read_file(context: Context, id: DataTransferId, file_index: usize, file: &File) {
    let read = JsFuture::from(file.array_buffer());

    wasm_bindgen_futures::spawn_local(async move {
        let bytes = match read.await {
            Ok(buffer) => Some(Uint8Array::new(&buffer).to_vec()),
            Err(error) => {
                tracing::warn!("dropped file could not be read: {error:?}");
                None
            },
        };

        let mut state = context.dnd.0.borrow_mut();
        let Some(incoming) = state.incoming(id) else { return };
        let Some(position) =
            incoming.types.iter().position(|type_| type_.file_index() == Some(file_index))
        else {
            return;
        };
        let slot = match (bytes, incoming.types.get(position)) {
            (Some(bytes), Some(type_)) => {
                Slot::Ready(Arc::new(WebData::new(type_.clone(), Payload::Bytes(bytes))))
            },
            _ => Slot::Unavailable,
        };
        if let Some(target) = incoming.slots.get_mut(position) {
            *target = slot;
        }
        let received = take_ready(incoming);
        drop(state);

        context.runner.send_events(received);
    });
}

fn on_pointer_down(context: &Context, _event: &PointerEvent) {
    context.dnd.0.borrow_mut().pressed = Some((context.window_id, context.canvas.clone()));
}

/// Ends an outgoing drag that the browser never started.
fn on_pointer_release(context: &Context, _event: &PointerEvent) {
    let mut state = context.dnd.0.borrow_mut();
    state.pressed = None;
    let Some(outgoing) = state.outgoing.take_if(|outgoing| !outgoing.started) else { return };
    drop(state);

    outgoing.canvas.set_draggable(false);
    context.runner.send_event(Event::WindowEvent {
        window_id: outgoing.window_id,
        event: WindowEvent::OutgoingDragCanceled { id: outgoing.id },
    });
}

fn on_drag_start(context: &Context, event: &DragEvent) {
    let mut state = context.dnd.0.borrow_mut();
    let Some(outgoing) = state
        .outgoing
        .as_mut()
        .filter(|outgoing| outgoing.window_id == context.window_id && !outgoing.started)
    else {
        event.prevent_default();
        return;
    };
    let Some(data) = event.data_transfer() else {
        event.prevent_default();
        return;
    };

    outgoing.started = true;
    fill(&data, &*outgoing.data);
    data.set_effect_allowed(dnd::effect_allowed(&outgoing.actions));
    if let Some(icon) = outgoing.icon.take() {
        set_drag_image(context, &data, &icon);
    }
}

fn on_drag_end(context: &Context, event: &DragEvent) {
    let mut state = context.dnd.0.borrow_mut();
    state.pressed = None;
    let Some(outgoing) = state
        .outgoing
        .take_if(|outgoing| outgoing.window_id == context.window_id && outgoing.started)
    else {
        return;
    };
    drop(state);

    outgoing.canvas.set_draggable(false);
    let action =
        event.data_transfer().and_then(|data| dnd::action_of_drop_effect(&data.drop_effect()));
    let event = match action {
        Some(action) => WindowEvent::OutgoingDragDropped { id: outgoing.id, action: Some(action) },
        None => WindowEvent::OutgoingDragCanceled { id: outgoing.id },
    };
    context.runner.send_event(Event::WindowEvent { window_id: outgoing.window_id, event });
}

/// Puts the text types of an outgoing drag into the `DataTransfer` of `dragstart`.
fn fill(data: &DataTransfer, send: &dyn DataTransferSend) {
    for type_ in send.available_types() {
        let Some(mime) = type_.hint().and_then(dnd::string_mime_of_hint) else {
            tracing::debug!("{type_:?} is not a text type and is not sent on the web");
            continue;
        };

        let text = match send.data_for_type(type_) {
            Some(SendData::String(text)) => text,
            Some(SendData::Uris(uris)) => dnd::join_uri_list(&uris),
            Some(SendData::Bytes(bytes)) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!("{mime} is not UTF-8: {error}");
                    continue;
                },
            },
            _ => continue,
        };
        if let Err(error) = data.set_data(mime, &text) {
            tracing::warn!("{mime} could not be set: {error:?}");
        }
    }
}

/// Shows an icon under the pointer while dragging.
fn set_drag_image(context: &Context, data: &DataTransfer, icon: &DragIcon) {
    let Some(rgba) = icon.icon.cast_ref::<RgbaIcon>() else {
        tracing::warn!("DragIcon::icon must be an RgbaIcon on the web; ignoring");
        return;
    };

    match icon_element(&context.document, rgba) {
        Ok(element) => {
            let x = icon.offset_x.checked_neg().unwrap_or(i32::MAX);
            let y = icon.offset_y.checked_neg().unwrap_or(i32::MAX);
            data.set_drag_image(&element, x, y);
            remove_later(&context.window, element);
        },
        Err(error) => tracing::warn!("drag icon could not be drawn: {error:?}"),
    }
}

/// Draws an icon into a canvas placed outside the visible page.
fn icon_element(document: &Document, rgba: &RgbaIcon) -> Result<HtmlElement, JsValue> {
    let element: HtmlCanvasElement = document.create_element("canvas")?.unchecked_into();
    element.set_attribute("width", &rgba.width().to_string())?;
    element.set_attribute("height", &rgba.height().to_string())?;
    element.set_attribute("style", "position:fixed;left:-100000px;top:-100000px;")?;

    let pixels = Uint8ClampedArray::new_with_length(
        u32::try_from(rgba.buffer().len()).map_err(|_| JsValue::from_str("icon too large"))?,
    );
    pixels.copy_from(rgba.buffer());
    let image = ImageData::new_with_js_u8_clamped_array(&pixels, rgba.width())?;
    let context: CanvasRenderingContext2d = element
        .get_context("2d")?
        .ok_or_else(|| JsValue::from_str("no 2d context"))?
        .unchecked_into();
    context.put_image_data(&image, 0.0, 0.0)?;

    document.body().ok_or_else(|| JsValue::from_str("no body"))?.append_child(&element)?;

    Ok(element.unchecked_into())
}

/// Removes an element once the current event is done.
fn remove_later(window: &web_sys::Window, element: HtmlElement) {
    let remove = Closure::once_into_js(move || element.remove());
    if let Err(error) = window.set_timeout_with_callback(remove.unchecked_ref()) {
        tracing::warn!("drag icon could not be removed: {error:?}");
    }
}
