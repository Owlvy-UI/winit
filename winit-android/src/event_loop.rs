use std::cell::Cell;
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use android_activity::input::{Axis, InputEvent, KeyAction, Keycode, MotionAction, MotionEvent, ToolType};
use android_activity::{
    AndroidApp, AndroidAppWaker, ConfigurationRef, InputStatus, MainEvent, Rect,
};
use dpi::{PhysicalInsets, PhysicalPosition, PhysicalSize, Position, Size};
use tracing::{debug, trace, warn};
use winit_core::application::ApplicationHandler;
use winit_core::cursor::{Cursor, CustomCursor, CustomCursorSource};
use winit_core::error::{EventLoopError, NotSupportedError, RequestError};
use winit_core::event::{self, DeviceId, StartCause, SurfaceSizeWriter};
use winit_core::event_loop::pump_events::PumpStatus;
use winit_core::data_transfer::{DataTransfer, DataTransferId, DataTransferSend, TransferType};
use winit_core::event_loop::{
    ActiveEventLoop as RootActiveEventLoop, AsyncRequestSerial, ControlFlow, DeviceEvents,
    DndAction, DragIcon, EventLoopProvider, EventLoopProxy as CoreEventLoopProxy,
    EventLoopProxyProvider, OwnedDisplayHandle as CoreOwnedDisplayHandle,
};
use winit_core::icon::RgbaIcon;
use winit_core::monitor::{Fullscreen, MonitorHandle as CoreMonitorHandle};
use winit_core::window::{
    self, CursorGrabMode, ImeCapabilities, ImePurpose, ImeRequest, ImeRequestError,
    ResizeDirection, Theme, Window as CoreWindow, WindowAttributes, WindowButtons, WindowId,
    WindowLevel,
};

use crate::{dnd, drag_and_drop, keycodes, pointer};

static HAS_FOCUS: AtomicBool = AtomicBool::new(true);

/// Returns the minimum `Option<Duration>`, taking into account that `None`
/// equates to an infinite timeout, not a zero timeout (so can't just use
/// `Option::min`)
fn min_timeout(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    a.map_or(b, |a_timeout| b.map_or(Some(a_timeout), |b_timeout| Some(a_timeout.min(b_timeout))))
}

#[derive(Clone, Debug)]
struct SharedFlagSetter {
    flag: Arc<AtomicBool>,
}
impl SharedFlagSetter {
    fn set(&self) -> bool {
        self.flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    }
}

#[derive(Debug)]
struct SharedFlag {
    flag: Arc<AtomicBool>,
}

// Used for queuing redraws from arbitrary threads. We don't care how many
// times a redraw is requested (so don't actually need to queue any data,
// we just need to know at the start of a main loop iteration if a redraw
// was queued and be able to read and clear the state atomically)
impl SharedFlag {
    fn new() -> Self {
        Self { flag: Arc::new(AtomicBool::new(false)) }
    }

    fn setter(&self) -> SharedFlagSetter {
        SharedFlagSetter { flag: self.flag.clone() }
    }

    fn get_and_reset(&self) -> bool {
        self.flag.swap(false, std::sync::atomic::Ordering::AcqRel)
    }
}

#[derive(Clone)]
struct RedrawRequester {
    flag: SharedFlagSetter,
    waker: AndroidAppWaker,
}

impl fmt::Debug for RedrawRequester {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedrawRequester").field("flag", &self.flag).finish_non_exhaustive()
    }
}

impl RedrawRequester {
    fn new(flag: &SharedFlag, waker: AndroidAppWaker) -> Self {
        RedrawRequester { flag: flag.setter(), waker }
    }

    fn request_redraw(&self) {
        if self.flag.set() {
            // Only explicitly try to wake up the main loop when the flag
            // value changes
            self.waker.wake();
        }
    }
}

#[derive(Debug)]
pub struct EventLoop {
    pub android_app: AndroidApp,
    window_target: ActiveEventLoop,
    redraw_flag: SharedFlag,
    loop_running: bool, // Dispatched `NewEvents<Init>`
    running: bool,
    pending_redraw: bool,
    cause: StartCause,
    pointers: pointer::Pointers,
    ignore_volume_keys: bool,
    combining_accent: Option<char>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlatformSpecificEventLoopAttributes {
    pub android_app: Option<AndroidApp>,
    pub ignore_volume_keys: bool,
}

impl Default for PlatformSpecificEventLoopAttributes {
    fn default() -> Self {
        Self { android_app: Default::default(), ignore_volume_keys: true }
    }
}

// Android currently only supports one window
const GLOBAL_WINDOW: WindowId = WindowId::from_raw(0);

impl EventLoop {
    pub fn new(attributes: &PlatformSpecificEventLoopAttributes) -> Result<Self, EventLoopError> {
        static EVENT_LOOP_CREATED: AtomicBool = AtomicBool::new(false);
        if EVENT_LOOP_CREATED.swap(true, Ordering::Relaxed) {
            // For better cross-platformness.
            return Err(EventLoopError::RecreationAttempt);
        }

        let android_app = attributes.android_app.as_ref().expect(
            "An `AndroidApp` as passed to android_main() is required to create an `EventLoop` on \
             Android",
        );

        let event_loop_proxy = Arc::new(EventLoopProxy::new(android_app.create_waker()));

        let redraw_flag = SharedFlag::new();
        drag_and_drop::init(android_app.create_waker());

        Ok(Self {
            android_app: android_app.clone(),
            pointers: pointer::Pointers::default(),
            window_target: ActiveEventLoop {
                app: android_app.clone(),
                control_flow: Cell::new(ControlFlow::default()),
                exit: Cell::new(false),
                redraw_requester: RedrawRequester::new(&redraw_flag, android_app.create_waker()),
                event_loop_proxy,
            },
            redraw_flag,
            loop_running: false,
            running: false,
            pending_redraw: false,
            cause: StartCause::Init,
            ignore_volume_keys: attributes.ignore_volume_keys,
            combining_accent: None,
        })
    }

    pub fn window_target(&self) -> &dyn RootActiveEventLoop {
        &self.window_target
    }

    fn single_iteration<A: ApplicationHandler>(
        &mut self,
        main_event: Option<MainEvent<'_>>,
        app: &mut A,
    ) {
        trace!("Mainloop iteration");

        let cause = self.cause;
        let mut pending_redraw = self.pending_redraw;
        let mut resized = false;

        app.new_events(&self.window_target, cause);

        if let Some(event) = main_event {
            trace!("Handling main event {:?}", event);

            match event {
                MainEvent::InitWindow { .. } => {
                    app.can_create_surfaces(&self.window_target);
                },
                MainEvent::TerminateWindow { .. } => {
                    app.destroy_surfaces(&self.window_target);
                },
                MainEvent::WindowResized { .. } => resized = true,
                MainEvent::RedrawNeeded { .. } => pending_redraw = true,
                MainEvent::ContentRectChanged { .. } => {
                    warn!("TODO: find a way to notify application of content rect change");
                },
                MainEvent::GainedFocus => {
                    HAS_FOCUS.store(true, Ordering::Relaxed);
                    let event = event::WindowEvent::Focused(true);
                    app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                },
                MainEvent::LostFocus => {
                    HAS_FOCUS.store(false, Ordering::Relaxed);
                    let event = event::WindowEvent::Focused(false);
                    app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                },
                MainEvent::ConfigChanged { .. } => {
                    let old_scale_factor = scale_factor(&self.android_app);
                    let scale_factor = scale_factor(&self.android_app);
                    if (scale_factor - old_scale_factor).abs() < f64::EPSILON {
                        let new_surface_size = Arc::new(Mutex::new(screen_size(&self.android_app)));
                        let event = event::WindowEvent::ScaleFactorChanged {
                            surface_size_writer: SurfaceSizeWriter::new(Arc::downgrade(
                                &new_surface_size,
                            )),
                            scale_factor,
                        };

                        app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                    }
                },
                MainEvent::LowMemory => {
                    app.memory_warning(&self.window_target);
                },
                MainEvent::Start => {
                    drag_and_drop::attach(&self.android_app);
                    app.resumed(self.window_target());
                },
                MainEvent::Resume { .. } => {
                    debug!("App Resumed - is running");
                    // TODO: This is incorrect - will be solved in https://github.com/rust-windowing/winit/pull/3897
                    self.running = true;
                },
                MainEvent::SaveState { .. } => {
                    // XXX: how to forward this state to applications?
                    // XXX: also how do we expose state restoration to apps?
                    warn!("TODO: forward saveState notification to application");
                },
                MainEvent::Pause => {
                    debug!("App Paused - stopped running");
                    // TODO: This is incorrect - will be solved in https://github.com/rust-windowing/winit/pull/3897
                    self.running = false;
                },
                MainEvent::Stop => {
                    app.suspended(self.window_target());
                },
                MainEvent::Destroy => {
                    // XXX: maybe exit mainloop to drop things before being
                    // killed by the OS?
                    warn!("TODO: forward onDestroy notification to application");
                },
                MainEvent::InsetsChanged { .. } => {
                    // XXX: how to forward this state to applications?
                    warn!("TODO: handle Android InsetsChanged notification");
                },
                unknown => {
                    trace!("Unknown MainEvent {unknown:?} (ignored)");
                },
            }
        } else {
            trace!("No main event to handle");
        }

        // temporarily decouple `android_app` from `self` so we aren't holding
        // a borrow of `self` while iterating
        let android_app = self.android_app.clone();

        // Process input events
        match android_app.input_events_iter() {
            Ok(mut input_iter) => loop {
                let read_event =
                    input_iter.next(|event| self.handle_input_event(&android_app, event, app));

                if !read_event {
                    break;
                }
            },
            Err(err) => {
                tracing::warn!("Failed to get input events iterator: {err:?}");
            },
        }

        for event in self.pointers.finish() {
            app.window_event(&self.window_target, GLOBAL_WINDOW, event);
        }

        for event in drag_and_drop::take_events() {
            app.window_event(&self.window_target, GLOBAL_WINDOW, event);
        }

        if self.window_target.event_loop_proxy.wake_up.swap(false, Ordering::Relaxed) {
            app.proxy_wake_up(&self.window_target);
        }

        if self.running {
            if resized {
                let size = if let Some(native_window) = self.android_app.native_window().as_ref() {
                    let width = native_window.width() as _;
                    let height = native_window.height() as _;
                    PhysicalSize::new(width, height)
                } else {
                    PhysicalSize::new(0, 0)
                };
                let event = event::WindowEvent::SurfaceResized(size);
                app.window_event(&self.window_target, GLOBAL_WINDOW, event);
            }

            pending_redraw |= self.redraw_flag.get_and_reset();
            if pending_redraw {
                pending_redraw = false;
                let event = event::WindowEvent::RedrawRequested;
                app.window_event(&self.window_target, GLOBAL_WINDOW, event);
            }
        }

        // This is always the last event we dispatch before poll again
        app.about_to_wait(&self.window_target);

        self.pending_redraw = pending_redraw;
    }

    fn handle_input_event<A: ApplicationHandler>(
        &mut self,
        android_app: &AndroidApp,
        event: &InputEvent<'_>,
        app: &mut A,
    ) -> InputStatus {
        let mut input_status = InputStatus::Handled;
        match event {
            InputEvent::MotionEvent(motion_event) => {
                if matches!(
                    motion_event.action(),
                    MotionAction::HoverEnter | MotionAction::HoverMove | MotionAction::HoverExit
                ) {
                    // Hover reaches the view hierarchy, where accessibility
                    // turns it into the node under the pointer.
                    input_status = InputStatus::Unhandled;
                }

                let motion = motion_of(motion_event);
                trace!("Input event {motion:?}");
                for event in self.pointers.handle(&motion) {
                    app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                }
            },
            InputEvent::KeyEvent(key) => {
                match key.key_code() {
                    // Flag keys related to volume as unhandled. While winit does not have a way for
                    // applications to configure what keys to flag as handled,
                    // this appears to be a good default until winit
                    // can be configured.
                    Keycode::VolumeUp | Keycode::VolumeDown | Keycode::VolumeMute
                        if self.ignore_volume_keys =>
                    {
                        input_status = InputStatus::Unhandled
                    },
                    keycode => {
                        let state = match key.action() {
                            KeyAction::Down => event::ElementState::Pressed,
                            KeyAction::Up => event::ElementState::Released,
                            _ => event::ElementState::Released,
                        };

                        let key_char = keycodes::character_map_and_combine_key(
                            android_app,
                            key,
                            &mut self.combining_accent,
                        );

                        let logical_key = keycodes::to_logical(key_char, keycode);
                        let text = if state == event::ElementState::Pressed {
                            logical_key.to_text().map(smol_str::SmolStr::new)
                        } else {
                            None
                        };

                        let event = event::WindowEvent::KeyboardInput {
                            device_id: Some(DeviceId::from_raw(key.device_id() as i64)),
                            event: event::KeyEvent {
                                state,
                                physical_key: keycodes::to_physical_key(keycode),
                                logical_key,
                                location: keycodes::to_location(keycode),
                                repeat: key.repeat_count() > 0,
                                text: text.clone(),
                                text_with_all_modifiers: text,
                                key_without_modifiers: keycodes::to_logical(key_char, keycode),
                            },
                            is_synthetic: false,
                        };

                        app.window_event(&self.window_target, GLOBAL_WINDOW, event);
                    },
                }
            },
            _ => {
                warn!("Unknown android_activity input event {event:?}")
            },
        }

        input_status
    }

    pub fn run_app_on_demand<A: ApplicationHandler>(
        &mut self,
        mut app: A,
    ) -> Result<(), EventLoopError> {
        self.window_target.clear_exit();
        loop {
            match self.pump_app_events(None, &mut app) {
                PumpStatus::Exit(0) => {
                    break Ok(());
                },
                PumpStatus::Exit(code) => {
                    break Err(EventLoopError::ExitFailure(code));
                },
                _ => {
                    continue;
                },
            }
        }
    }

    pub fn pump_app_events<A: ApplicationHandler>(
        &mut self,
        timeout: Option<Duration>,
        mut app: A,
    ) -> PumpStatus {
        if !self.loop_running {
            self.loop_running = true;

            // Reset the internal state for the loop as we start running to
            // ensure consistent behaviour in case the loop runs and exits more
            // than once
            self.pending_redraw = false;
            self.cause = StartCause::Init;

            // run the initial loop iteration
            self.single_iteration(None, &mut app);
        }

        // Consider the possibility that the `StartCause::Init` iteration could
        // request to Exit
        if !self.exiting() {
            self.poll_events_with_timeout(timeout, &mut app);
        }
        if self.exiting() {
            self.loop_running = false;

            PumpStatus::Exit(0)
        } else {
            PumpStatus::Continue
        }
    }

    fn poll_events_with_timeout<A: ApplicationHandler>(
        &mut self,
        mut timeout: Option<Duration>,
        app: &mut A,
    ) {
        let start = Instant::now();

        self.pending_redraw |= self.redraw_flag.get_and_reset();

        timeout = if self.running
            && (self.pending_redraw
                || self.window_target.event_loop_proxy.wake_up.load(Ordering::Relaxed))
        {
            // If we already have work to do then we don't want to block on the next poll
            Some(Duration::ZERO)
        } else {
            let control_flow_timeout = match self.control_flow() {
                ControlFlow::Wait => None,
                ControlFlow::Poll => Some(Duration::ZERO),
                ControlFlow::WaitUntil(wait_deadline) => {
                    Some(wait_deadline.saturating_duration_since(start))
                },
            };

            min_timeout(control_flow_timeout, timeout)
        };

        let android_app = self.android_app.clone(); // Don't borrow self as part of poll expression
        android_app.poll_events(timeout, |poll_event| {
            let mut main_event = None;

            match poll_event {
                android_activity::PollEvent::Wake => {
                    // In the X11 backend it's noted that too many false-positive wake ups
                    // would cause the event loop to run continuously. They handle this by
                    // re-checking for pending events (assuming they cover all
                    // valid reasons for a wake up).
                    //
                    // For now, user_events and redraw_requests are the only reasons to expect
                    // a wake up here so we can ignore the wake up if there are no events/requests.
                    // We also ignore wake ups while suspended.
                    self.pending_redraw |= self.redraw_flag.get_and_reset();
                    if !drag_and_drop::pending()
                        && (!self.running
                            || (!self.pending_redraw
                                && !self
                                    .window_target
                                    .event_loop_proxy
                                    .wake_up
                                    .load(Ordering::Relaxed)))
                    {
                        return;
                    }
                },
                android_activity::PollEvent::Timeout => {},
                android_activity::PollEvent::Main(event) => {
                    main_event = Some(event);
                },
                unknown_event => {
                    warn!("Unknown poll event {unknown_event:?} (ignored)");
                },
            }

            self.cause = match self.control_flow() {
                ControlFlow::Poll => StartCause::Poll,
                ControlFlow::Wait => StartCause::WaitCancelled { start, requested_resume: None },
                ControlFlow::WaitUntil(deadline) => {
                    if Instant::now() < deadline {
                        StartCause::WaitCancelled { start, requested_resume: Some(deadline) }
                    } else {
                        StartCause::ResumeTimeReached { start, requested_resume: deadline }
                    }
                },
            };

            self.single_iteration(main_event, app);
        });
    }

    fn control_flow(&self) -> ControlFlow {
        self.window_target.control_flow()
    }

    fn exiting(&self) -> bool {
        self.window_target.exiting()
    }
}

impl EventLoopProvider for EventLoop {
    fn run_app<A: ApplicationHandler + 'static>(
        mut self,
        mut app: A,
    ) -> Result<(), EventLoopError> {
        let result = self.run_app_on_demand(&mut app);
        // SAFETY: unsure that the state is dropped before the exit from the event loop.
        drop(app);
        result
    }

    fn create_proxy(&self) -> CoreEventLoopProxy {
        self.window_target().create_proxy()
    }

    fn owned_display_handle(&self) -> CoreOwnedDisplayHandle {
        self.window_target().owned_display_handle()
    }

    fn listen_device_events(&self, allowed: DeviceEvents) {
        self.window_target().listen_device_events(allowed);
    }

    fn set_control_flow(&self, control_flow: ControlFlow) {
        self.window_target().set_control_flow(control_flow);
    }

    fn create_custom_cursor(
        &self,
        custom_cursor: CustomCursorSource,
    ) -> Result<CustomCursor, RequestError> {
        self.window_target().create_custom_cursor(custom_cursor)
    }
}

pub struct EventLoopProxy {
    wake_up: AtomicBool,
    waker: AndroidAppWaker,
}

impl fmt::Debug for EventLoopProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventLoopProxy").field("wake_up", &self.wake_up).finish_non_exhaustive()
    }
}

impl EventLoopProxy {
    fn new(waker: AndroidAppWaker) -> Self {
        Self { wake_up: AtomicBool::new(false), waker }
    }
}

impl EventLoopProxyProvider for EventLoopProxy {
    fn wake_up(&self) {
        self.wake_up.store(true, Ordering::Relaxed);
        self.waker.wake();
    }
}

#[derive(Debug)]
pub struct ActiveEventLoop {
    pub(crate) app: AndroidApp,
    control_flow: Cell<ControlFlow>,
    exit: Cell<bool>,
    redraw_requester: RedrawRequester,
    event_loop_proxy: Arc<EventLoopProxy>,
}

impl ActiveEventLoop {
    fn clear_exit(&self) {
        self.exit.set(false);
    }
}

impl RootActiveEventLoop for ActiveEventLoop {
    fn create_proxy(&self) -> CoreEventLoopProxy {
        CoreEventLoopProxy::new(self.event_loop_proxy.clone())
    }

    fn create_window(
        &self,
        window_attributes: WindowAttributes,
    ) -> Result<Box<dyn CoreWindow>, RequestError> {
        Ok(Box::new(Window::new(self, window_attributes)?))
    }

    fn create_custom_cursor(
        &self,
        _source: CustomCursorSource,
    ) -> Result<CustomCursor, RequestError> {
        Err(NotSupportedError::new("create_custom_cursor is not supported").into())
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(std::iter::empty())
    }

    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        None
    }

    fn system_theme(&self) -> Option<Theme> {
        None
    }

    fn listen_device_events(&self, _allowed: DeviceEvents) {}

    fn set_control_flow(&self, control_flow: ControlFlow) {
        self.control_flow.set(control_flow)
    }

    fn control_flow(&self) -> ControlFlow {
        self.control_flow.get()
    }

    fn exit(&self) {
        self.exit.set(true)
    }

    fn exiting(&self) -> bool {
        self.exit.get()
    }

    fn owned_display_handle(&self) -> CoreOwnedDisplayHandle {
        CoreOwnedDisplayHandle::new(Arc::new(OwnedDisplayHandle))
    }

    fn rwh_06_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }

    fn fetch_data_transfer(
        &self,
        id: DataTransferId,
        type_: &dyn TransferType,
    ) -> Result<AsyncRequestSerial, RequestError> {
        dnd_result(drag_and_drop::with_dnd(|dnd| dnd.fetch(id, type_)))
    }

    fn data_transfer(&self, id: DataTransferId) -> Result<Box<dyn DataTransfer>, RequestError> {
        dnd_result(drag_and_drop::with_dnd(|dnd| dnd.data_transfer(id)))
    }

    fn set_valid_dnd_actions(
        &self,
        id: DataTransferId,
        actions: &[DndAction],
    ) -> Result<(), RequestError> {
        dnd_result(drag_and_drop::with_dnd(|dnd| dnd.set_valid(id, actions)))
    }

    fn start_drag(
        &self,
        source: WindowId,
        send_data: Box<dyn DataTransferSend>,
        actions: &[DndAction],
        icon: Option<DragIcon>,
    ) -> Result<DataTransferId, RequestError> {
        if source != GLOBAL_WINDOW {
            return Err(RequestError::Ignored);
        }

        let clip = dnd_result(Some(dnd::outgoing_clip(&*send_data, actions)))?;
        let shadow = icon.as_ref().and_then(drag_shadow);
        let id = drag_and_drop::with_dnd(dnd::Dnd::start).ok_or(RequestError::Ignored)?;
        if let Err(error) = drag_and_drop::start(&self.app, id.into_raw(), &clip, shadow.as_ref()) {
            warn!("the drag could not be handed to the UI thread: {error}");
            drag_and_drop::with_dnd(|dnd| dnd.abandon(id));
            return Err(RequestError::Ignored);
        }

        Ok(id)
    }
}

/// Copies what the pointers need out of a motion event.
fn motion_of(event: &MotionEvent<'_>) -> pointer::Motion {
    let phase = match event.action() {
        MotionAction::Down | MotionAction::PointerDown => {
            pointer::Phase::Down(event.pointer_index())
        },
        MotionAction::Up | MotionAction::PointerUp => pointer::Phase::Up(event.pointer_index()),
        MotionAction::Move => pointer::Phase::Move,
        MotionAction::Cancel => pointer::Phase::Cancel,
        MotionAction::HoverEnter => pointer::Phase::HoverEnter,
        MotionAction::HoverMove => pointer::Phase::HoverMove,
        MotionAction::HoverExit => pointer::Phase::HoverExit,
        MotionAction::ButtonPress | MotionAction::ButtonRelease => pointer::Phase::Button,
        MotionAction::Scroll => pointer::Phase::Scroll,
        _ => pointer::Phase::Other,
    };
    let contacts = event
        .pointers()
        .map(|pointer| pointer::Contact {
            id: usize::try_from(pointer.pointer_id()).unwrap_or(usize::MAX),
            tool: match pointer.tool_type() {
                ToolType::Finger => pointer::Tool::Finger,
                ToolType::Mouse => pointer::Tool::Mouse,
                ToolType::Stylus => pointer::Tool::Stylus,
                ToolType::Eraser => pointer::Tool::Eraser,
                _ => pointer::Tool::Unknown,
            },
            position: PhysicalPosition::new(f64::from(pointer.x()), f64::from(pointer.y())),
            pressure: pointer.pressure(),
            tilt: pointer.axis_value(Axis::Tilt),
            orientation: pointer.orientation(),
            hscroll: pointer.axis_value(Axis::Hscroll),
            vscroll: pointer.axis_value(Axis::Vscroll),
        })
        .collect();

    pointer::Motion {
        device: i64::from(event.device_id()),
        phase,
        buttons: event.button_state().0,
        contacts,
    }
}

/// The shadow of a drag icon, which must be an [`RgbaIcon`].
fn drag_shadow(icon: &DragIcon) -> Option<dnd::Shadow> {
    let Some(rgba) = icon.icon.cast_ref::<RgbaIcon>() else {
        warn!("DragIcon::icon must be an RgbaIcon on Android; ignoring");
        return None;
    };

    let shadow =
        dnd::shadow(rgba.buffer(), rgba.width(), rgba.height(), icon.offset_x, icon.offset_y);
    if shadow.is_none() {
        warn!("the drag icon is empty or larger than {0}x{0}; ignoring", dnd::MAX_SHADOW_SIDE);
    }

    shadow
}

/// Turns the answer of the drag state into the error a request returns.
fn dnd_result<T>(result: Option<Result<T, dnd::DndError>>) -> Result<T, RequestError> {
    match result {
        Some(Ok(value)) => Ok(value),
        Some(Err(
            dnd::DndError::UnknownTransfer
            | dnd::DndError::UnknownType
            | dnd::DndError::TooManyFetches,
        ))
        | None => Err(RequestError::Ignored),
        Some(Err(error)) => Err(NotSupportedError::new(error.message()).into()),
    }
}

impl rwh_06::HasDisplayHandle for ActiveEventLoop {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::AndroidDisplayHandle::new();
        Ok(unsafe { rwh_06::DisplayHandle::borrow_raw(raw.into()) })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OwnedDisplayHandle;

impl rwh_06::HasDisplayHandle for OwnedDisplayHandle {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::AndroidDisplayHandle::new();
        Ok(unsafe { rwh_06::DisplayHandle::borrow_raw(raw.into()) })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlatformSpecificWindowAttributes;

#[derive(Debug)]
pub struct Window {
    app: AndroidApp,
    ime_capabilities: Mutex<Option<ImeCapabilities>>,
    redraw_requester: RedrawRequester,
}

impl Window {
    pub(crate) fn new(
        el: &ActiveEventLoop,
        window_attrs: window::WindowAttributes,
    ) -> Result<Self, RequestError> {
        if window_attrs.window_type() == window::WindowType::Popup {
            return Err(RequestError::NotSupported(NotSupportedError::new(
                "Popups are not implemented for Android",
            )));
        }

        // FIXME this ignores the rest of the requested window attributes

        Ok(Self {
            app: el.app.clone(),
            ime_capabilities: Default::default(),
            redraw_requester: el.redraw_requester.clone(),
        })
    }

    pub(crate) fn config(&self) -> ConfigurationRef {
        self.app.config()
    }

    pub(crate) fn content_rect(&self) -> Rect {
        self.app.content_rect()
    }

    // Allow the usage of HasRawWindowHandle inside this function
    #[allow(deprecated)]
    fn raw_window_handle_rwh_06(&self) -> Result<rwh_06::RawWindowHandle, rwh_06::HandleError> {
        use rwh_06::HasRawWindowHandle;

        if let Some(native_window) = self.app.native_window().as_ref() {
            native_window.raw_window_handle()
        } else {
            tracing::error!(
                "Cannot get the native window, it's null and will always be null before \
                 Event::Resumed and after Event::Suspended. Make sure you only call this function \
                 between those events."
            );
            Err(rwh_06::HandleError::Unavailable)
        }
    }

    fn raw_display_handle_rwh_06(&self) -> Result<rwh_06::RawDisplayHandle, rwh_06::HandleError> {
        Ok(rwh_06::RawDisplayHandle::Android(rwh_06::AndroidDisplayHandle::new()))
    }
}

impl rwh_06::HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = self.raw_display_handle_rwh_06()?;
        unsafe { Ok(rwh_06::DisplayHandle::borrow_raw(raw)) }
    }
}

impl rwh_06::HasWindowHandle for Window {
    fn window_handle(&self) -> Result<rwh_06::WindowHandle<'_>, rwh_06::HandleError> {
        let raw = self.raw_window_handle_rwh_06()?;
        unsafe { Ok(rwh_06::WindowHandle::borrow_raw(raw)) }
    }
}

impl CoreWindow for Window {
    fn window_type(&self) -> window::WindowType {
        window::WindowType::Window
    }

    fn id(&self) -> WindowId {
        GLOBAL_WINDOW
    }

    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        None
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(std::iter::empty())
    }

    fn current_monitor(&self) -> Option<CoreMonitorHandle> {
        None
    }

    fn scale_factor(&self) -> f64 {
        scale_factor(&self.app)
    }

    fn request_redraw(&self) {
        self.redraw_requester.request_redraw()
    }

    fn pre_present_notify(&self) {}

    fn surface_position(&self) -> PhysicalPosition<i32> {
        (0, 0).into()
    }

    fn outer_position(&self) -> Result<PhysicalPosition<i32>, RequestError> {
        Err(NotSupportedError::new("outer_position is not supported").into())
    }

    fn set_outer_position(&self, _position: Position) {
        // no effect
    }

    fn surface_size(&self) -> PhysicalSize<u32> {
        self.outer_size()
    }

    fn request_surface_size(&self, _size: Size) -> Option<PhysicalSize<u32>> {
        Some(self.surface_size())
    }

    fn outer_size(&self) -> PhysicalSize<u32> {
        screen_size(&self.app)
    }

    fn safe_area(&self) -> PhysicalInsets<u32> {
        PhysicalInsets::new(0, 0, 0, 0)
    }

    fn set_min_surface_size(&self, _: Option<Size>) {}

    fn set_max_surface_size(&self, _: Option<Size>) {}

    fn surface_resize_increments(&self) -> Option<PhysicalSize<u32>> {
        None
    }

    fn set_surface_resize_increments(&self, _increments: Option<Size>) {}

    fn set_title(&self, _title: &str) {}

    fn set_transparent(&self, _transparent: bool) {}

    fn set_blur(&self, _blur: bool) {}

    fn set_visible(&self, _visibility: bool) {}

    fn is_visible(&self) -> Option<bool> {
        None
    }

    fn set_resizable(&self, _resizeable: bool) {}

    fn is_resizable(&self) -> bool {
        false
    }

    fn set_enabled_buttons(&self, _buttons: WindowButtons) {}

    fn enabled_buttons(&self) -> WindowButtons {
        WindowButtons::all()
    }

    fn set_minimized(&self, _minimized: bool) {}

    fn is_minimized(&self) -> Option<bool> {
        None
    }

    fn set_maximized(&self, _maximized: bool) {}

    fn is_maximized(&self) -> bool {
        false
    }

    fn set_fullscreen(&self, _monitor: Option<Fullscreen>) {
        warn!("Cannot set fullscreen on Android");
    }

    fn fullscreen(&self) -> Option<Fullscreen> {
        None
    }

    fn set_decorations(&self, _decorations: bool) {}

    fn is_decorated(&self) -> bool {
        true
    }

    fn set_window_level(&self, _level: WindowLevel) {}

    fn set_window_icon(&self, _window_icon: Option<winit_core::icon::Icon>) {}

    fn set_ime_cursor_area(&self, _position: Position, _size: Size) {}

    fn request_ime_update(&self, request: ImeRequest) -> Result<(), ImeRequestError> {
        let mut current_caps = self.ime_capabilities.lock().unwrap();
        match request {
            ImeRequest::Enable(enable) => {
                let (capabilities, _) = enable.into_raw();
                if current_caps.is_some() {
                    return Err(ImeRequestError::AlreadyEnabled);
                }
                *current_caps = Some(capabilities);
                self.app.show_soft_input(true);
            },
            ImeRequest::Update(_) => {
                if current_caps.is_none() {
                    return Err(ImeRequestError::NotEnabled);
                }
            },
            ImeRequest::Disable => {
                *current_caps = None;
                self.app.hide_soft_input(true);
            },
            _ => return Err(ImeRequestError::NotSupported),
        }

        Ok(())
    }

    fn ime_capabilities(&self) -> Option<ImeCapabilities> {
        *self.ime_capabilities.lock().unwrap()
    }

    fn set_ime_purpose(&self, _purpose: ImePurpose) {}

    fn focus_window(&self) {}

    fn request_user_attention(&self, _request_type: Option<window::UserAttentionType>) {}

    fn set_cursor(&self, _: Cursor) {}

    fn set_cursor_position(&self, _: Position) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_position is not supported").into())
    }

    fn set_cursor_grab(&self, _: CursorGrabMode) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_grab is not supported").into())
    }

    fn set_cursor_visible(&self, _: bool) {}

    fn drag_window(&self) -> Result<(), RequestError> {
        Err(NotSupportedError::new("drag_window is not supported").into())
    }

    fn drag_resize_window(&self, _direction: ResizeDirection) -> Result<(), RequestError> {
        Err(NotSupportedError::new("drag_resize_window").into())
    }

    #[inline]
    fn show_window_menu(&self, _position: Position) {}

    fn set_cursor_hittest(&self, _hittest: bool) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_hittest is not supported").into())
    }

    fn set_theme(&self, _theme: Option<Theme>) {}

    fn theme(&self) -> Option<Theme> {
        None
    }

    fn set_content_protected(&self, _protected: bool) {}

    fn has_focus(&self) -> bool {
        HAS_FOCUS.load(Ordering::Relaxed)
    }

    fn title(&self) -> String {
        String::new()
    }

    fn reset_dead_keys(&self) {}

    fn rwh_06_display_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }

    fn rwh_06_window_handle(&self) -> &dyn rwh_06::HasWindowHandle {
        self
    }
}

fn screen_size(app: &AndroidApp) -> PhysicalSize<u32> {
    if let Some(native_window) = app.native_window() {
        PhysicalSize::new(native_window.width() as _, native_window.height() as _)
    } else {
        PhysicalSize::new(0, 0)
    }
}

fn scale_factor(app: &AndroidApp) -> f64 {
    app.config().density().map(|dpi| dpi as f64 / 160.0).unwrap_or(1.0)
}
