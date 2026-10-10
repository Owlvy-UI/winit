//! The Java side of drag and drop.
//!
//! `org.rustwindowing.winit.DragAndDrop` ships as `dnd.dex`, is loaded through
//! `dalvik.system.InMemoryDexClassLoader` and listens on the decor view of the activity. Its
//! native methods run on the UI thread, feed [`Dnd`] and wake the event loop, which hands the
//! queued events to the application.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use android_activity::{AndroidApp, AndroidAppWaker};
use jni::errors::LogErrorAndDefault;
use jni::objects::{Global, JClass, JIntArray, JObject, JObjectArray, JString, JValue, JValueOwned};
use jni::sys::{jboolean, jfloat, jint, jlong};
use jni::{Env, EnvUnowned, JavaVM, NativeMethod, jni_sig, jni_str};
use tracing::warn;
use winit_core::event::WindowEvent;

use crate::dnd::{self, ClipContent, Dnd, OutgoingClip, Shadow};

/// The compiled listener, built from `java/` by `build-dex.sh`.
const DEX: &[u8] = include_bytes!("../dnd.dex");

/// The name of the listener class, as `ClassLoader.loadClass` spells it.
const CLASS_NAME: &str = "org.rustwindowing.winit.DragAndDrop";

/// State shared between the UI thread and the event loop.
struct Shared {
    dnd: Mutex<Dnd>,
    pending: AtomicBool,
    waker: AndroidAppWaker,
}

/// The state of the one event loop of the process.
static SHARED: OnceLock<Shared> = OnceLock::new();

/// The listener class, loaded on the first attach.
static CLASS: OnceLock<Global<JClass<'static>>> = OnceLock::new();

/// Sets up the shared state for the event loop woken by `waker`.
pub(crate) fn init(waker: AndroidAppWaker) {
    SHARED.get_or_init(|| Shared { dnd: Mutex::new(Dnd::default()), pending: AtomicBool::new(false), waker });
}

/// Runs `f` on the drag state and wakes the event loop when events wait.
///
/// Returns `None` before [`init`] or when the state was poisoned.
pub(crate) fn with_dnd<T>(f: impl FnOnce(&mut Dnd) -> T) -> Option<T> {
    let Some(shared) = SHARED.get() else {
        warn!("drag and drop is used before the event loop exists");
        return None;
    };

    let Ok(mut dnd) = shared.dnd.lock() else {
        warn!("the drag and drop state is poisoned");
        return None;
    };

    let value = f(&mut dnd);
    let wake = dnd.has_events();
    drop(dnd);
    if wake {
        shared.pending.store(true, Ordering::Release);
        shared.waker.wake();
    }

    Some(value)
}

/// Whether drag events wait for the event loop.
pub(crate) fn pending() -> bool {
    SHARED.get().is_some_and(|shared| shared.pending.load(Ordering::Acquire))
}

/// Takes the drag events waiting for the event loop.
pub(crate) fn take_events() -> VecDeque<WindowEvent> {
    let Some(shared) = SHARED.get() else { return VecDeque::new() };
    shared.pending.store(false, Ordering::Release);
    match shared.dnd.lock() {
        Ok(mut dnd) => dnd.take_events(),
        Err(_poisoned) => {
            warn!("the drag and drop state is poisoned");
            VecDeque::new()
        },
    }
}

/// Puts the listener on the decor view of the current activity.
pub(crate) fn attach(app: &AndroidApp) {
    let attached = call(app, |env, class, activity| {
        env.call_static_method(
            class,
            jni_str!("attach"),
            jni_sig!("(Landroid/app/Activity;)V"),
            &[JValue::Object(activity)],
        )
        .map(drop)
    });

    if let Err(error) = attached {
        warn!("the drag listener could not be attached: {error}");
    }
}

/// Hands an outgoing drag to the UI thread.
///
/// # Errors
///
/// Returns what the virtual machine reported while the request was handed over.
pub(crate) fn start(
    app: &AndroidApp,
    drag: i64,
    clip: &OutgoingClip,
    shadow: Option<&Shadow>,
) -> Result<(), jni::errors::Error> {
    call(app, |env, class, activity| {
        let text = optional_string(env, clip.text.as_deref())?;
        let html = optional_string(env, clip.html.as_deref())?;
        let uris = JObjectArray::<JString>::new(env, clip.uris.len(), JString::null())?;
        for (index, uri) in clip.uris.iter().enumerate() {
            let uri = env.new_string(uri)?;
            uris.set_element(env, index, &uri)?;
            env.delete_local_ref(uri);
        }

        let actions = JIntArray::new(env, clip.actions.len())?;
        actions.set_region(env, 0, &clip.actions)?;
        let (pixels, width, height, touch_x, touch_y) = match shadow {
            Some(shadow) => {
                let pixels = JIntArray::new(env, shadow.argb.len())?;
                pixels.set_region(env, 0, &shadow.argb)?;
                (pixels, shadow.width, shadow.height, shadow.touch_x, shadow.touch_y)
            },
            None => (JIntArray::null(), 0, 0, 0, 0),
        };

        env.call_static_method(
            class,
            jni_str!("start"),
            jni_sig!(
                "(Landroid/app/Activity;JLjava/lang/String;Ljava/lang/String;[Ljava/lang/String;[I[IIIII)V"
            ),
            &[
                JValue::Object(activity),
                JValue::Long(drag),
                JValue::Object(&text),
                JValue::Object(&html),
                JValue::Object(&uris),
                JValue::Object(&actions),
                JValue::Object(&pixels),
                JValue::Int(width),
                JValue::Int(height),
                JValue::Int(touch_x),
                JValue::Int(touch_y),
            ],
        )
        .map(drop)
    })
}

/// A Java string, or null for `None`.
fn optional_string<'local>(
    env: &mut Env<'local>,
    text: Option<&str>,
) -> Result<JObject<'local>, jni::errors::Error> {
    match text {
        Some(text) => Ok(env.new_string(text)?.into()),
        None => Ok(JObject::null()),
    }
}

/// Runs `f` with the listener class and the activity, on a thread attached to the machine.
fn call<T>(
    app: &AndroidApp,
    f: impl FnOnce(&mut Env<'_>, &Global<JClass<'static>>, &JObject<'_>) -> Result<T, jni::errors::Error>,
) -> Result<T, jni::errors::Error> {
    let vm: *mut c_void = app.vm_as_ptr();
    let activity: *mut c_void = app.activity_as_ptr();
    if vm.is_null() || activity.is_null() {
        return Err(jni::errors::Error::NullPtr("the virtual machine or the activity"));
    }

    // SAFETY: the pointer is the `JavaVM` the glue holds for the lifetime of the process.
    let vm = unsafe { JavaVM::from_raw(vm.cast()) };
    vm.attach_current_thread(|env| {
        let class = class(env)?;
        // SAFETY: the pointer is the global reference to the activity the glue holds while it
        // runs; the borrowed wrapper never releases it.
        let activity = unsafe { JObject::from_raw(env, activity.cast()) };
        let result = f(env, class, &activity);
        if result.is_err() && env.exception_check() {
            env.exception_clear();
        }

        result
    })
}

/// The listener class, loading the shipped dex on the first call.
fn class(env: &mut Env<'_>) -> Result<&'static Global<JClass<'static>>, jni::errors::Error> {
    if let Some(class) = CLASS.get() {
        return Ok(class);
    }

    let loaded = load(env)?;
    Ok(CLASS.get_or_init(|| loaded))
}

/// Loads the listener class out of the dex and registers its native methods.
fn load(env: &mut Env<'_>) -> Result<Global<JClass<'static>>, jni::errors::Error> {
    // SAFETY: the bytes are static, so they outlive every use the machine makes of the buffer.
    let buffer = unsafe { env.new_direct_byte_buffer(DEX.as_ptr().cast_mut(), DEX.len()) }?;
    let parent = JObject::null();
    let loader = env.new_object(
        jni_str!("dalvik/system/InMemoryDexClassLoader"),
        jni_sig!("(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V"),
        &[JValue::Object(&buffer), JValue::Object(&parent)],
    )?;
    let name = env.new_string(CLASS_NAME)?;
    let found = env
        .call_method(
            &loader,
            jni_str!("loadClass"),
            jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
            &[JValue::Object(&name)],
        )
        .and_then(JValueOwned::l)?;
    let found = JClass::cast_local(env, found)?;
    let natives = natives();
    // SAFETY: every pointer in `natives` is an `extern "system"` function whose parameters
    // match the descriptor next to it, and each descriptor matches the Java declaration.
    unsafe { env.register_native_methods(&found, &natives) }?;
    env.new_global_ref(&found)
}

/// The native methods the listener class declares.
fn natives() -> [NativeMethod<'static>; 6] {
    // SAFETY: each function below has the parameters its descriptor names, after the
    // environment and the class every static native method receives.
    unsafe {
        [
            NativeMethod::from_raw_parts(
                jni_str!("entered"),
                jni_str!("([Ljava/lang/String;[I)V"),
                entered as *mut c_void,
            ),
            NativeMethod::from_raw_parts(jni_str!("located"), jni_str!("(FF)V"), located as *mut c_void),
            NativeMethod::from_raw_parts(jni_str!("exited"), jni_str!("()V"), exited as *mut c_void),
            NativeMethod::from_raw_parts(
                jni_str!("dropped"),
                jni_str!("(FF[Ljava/lang/String;[Ljava/lang/String;[Ljava/lang/String;)I"),
                dropped as *mut c_void,
            ),
            NativeMethod::from_raw_parts(jni_str!("ended"), jni_str!("(ZJZI)V"), ended as *mut c_void),
            NativeMethod::from_raw_parts(jni_str!("failed"), jni_str!("(J)V"), failed as *mut c_void),
        ]
    }
}

/// Reads at most `max` strings of a Java array, at most [`dnd::MAX_BYTES`] in all.
fn strings(
    env: &mut Env<'_>,
    array: &JObjectArray<'_, JString<'_>>,
    max: usize,
) -> Result<Vec<String>, jni::errors::Error> {
    if array.is_null() {
        return Ok(Vec::new());
    }

    let mut strings = Vec::new();
    let mut bytes = 0_usize;
    for index in 0..array.len(env)?.min(max) {
        let element = array.get_element(env, index)?;
        if element.is_null() {
            continue;
        }

        let text = element.try_to_string(env)?;
        env.delete_local_ref(element);
        bytes = bytes.saturating_add(text.len());
        if bytes > dnd::MAX_BYTES {
            warn!("dropped text exceeds {} bytes; the rest is left out", dnd::MAX_BYTES);
            break;
        }

        strings.push(text);
    }

    Ok(strings)
}

/// Reads at most `max` ints of a Java array.
fn ints(env: &Env<'_>, array: &JIntArray<'_>, max: usize) -> Result<Vec<i32>, jni::errors::Error> {
    if array.is_null() {
        return Ok(Vec::new());
    }

    let mut ints = vec![0; array.len(env)?.min(max)];
    array.get_region(env, 0, &mut ints)?;
    Ok(ints)
}

extern "system" fn entered<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    mimes: JObjectArray<'local, JString<'local>>,
    actions: JIntArray<'local>,
) {
    unowned
        .with_env(|env| -> Result<(), jni::errors::Error> {
            let mimes = strings(env, &mimes, dnd::MAX_MIME_TYPES)?;
            let codes = ints(env, &actions, dnd::MAX_ACTIONS)?;
            with_dnd(|dnd| dnd.entered(&mimes, &codes));
            Ok(())
        })
        .resolve::<LogErrorAndDefault>();
}

extern "system" fn located(_unowned: EnvUnowned<'_>, _class: JClass<'_>, x: jfloat, y: jfloat) {
    with_dnd(|dnd| dnd.located(x, y));
}

extern "system" fn exited(_unowned: EnvUnowned<'_>, _class: JClass<'_>) {
    with_dnd(Dnd::exited);
}

extern "system" fn dropped<'local>(
    mut unowned: EnvUnowned<'local>,
    _class: JClass<'local>,
    x: jfloat,
    y: jfloat,
    texts: JObjectArray<'local, JString<'local>>,
    htmls: JObjectArray<'local, JString<'local>>,
    uris: JObjectArray<'local, JString<'local>>,
) -> jint {
    unowned
        .with_env(|env| -> Result<jint, jni::errors::Error> {
            let content = ClipContent {
                texts: strings(env, &texts, dnd::MAX_ITEMS)?,
                htmls: strings(env, &htmls, dnd::MAX_ITEMS)?,
                uris: strings(env, &uris, dnd::MAX_ITEMS)?,
            };
            Ok(with_dnd(|dnd| dnd.dropped(x, y, content)).unwrap_or(dnd::NO_ACTION))
        })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn ended(
    _unowned: EnvUnowned<'_>,
    _class: JClass<'_>,
    ours: jboolean,
    drag: jlong,
    result: jboolean,
    action: jint,
) {
    with_dnd(|dnd| dnd.ended(ours, drag, result, action));
}

extern "system" fn failed(_unowned: EnvUnowned<'_>, _class: JClass<'_>, drag: jlong) {
    with_dnd(|dnd| dnd.failed(drag));
}
