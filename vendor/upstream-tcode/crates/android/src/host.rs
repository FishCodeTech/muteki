//! Android services behind this client's `ClientHost`.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use android_activity::AndroidApp;
use futures::{StreamExt as _, channel::mpsc};
use gpui::App;
use jni::{
    Env, JavaVM, jni_sig, jni_str,
    objects::{JIntArray, JObject, JString, JValue},
    refs::Global,
};
use tcode_client::host::{HostFuture, PickedImage};
use tcode_traverse::NativeClientHost;

const RESULT_OK: i32 = 0;
const RESULT_CANCELLED: i32 = 1;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static EVENT_SENDER: LazyLock<Mutex<Option<mpsc::UnboundedSender<BridgeEvent>>>> =
    LazyLock::new(|| Mutex::new(None));

/// What the activity reports to the GPUI thread through [`EVENT_SENDER`].
enum BridgeEvent {
    CameraResult {
        request_id: u64,
        status: i32,
        value: Option<String>,
    },
    /// One image the picker returned; more may follow before `ImagesPicked`.
    ImagePicked {
        request_id: u64,
        image: PickedImage,
    },
    ImagesPicked {
        request_id: u64,
        status: i32,
        error: Option<String>,
    },
    NetworkChanged,
}

/// A picker request in flight: what arrived so far and who awaits it.
struct PickRequest {
    images: Vec<PickedImage>,
    done: async_channel::Sender<Result<Vec<PickedImage>, String>>,
}

#[derive(Clone)]
struct JniObject {
    vm: JavaVM,
    activity: Arc<Global<JObject<'static>>>,
}

impl JniObject {
    fn call_string(&self, method: &'static jni::strings::JNIStr) -> Result<Option<String>, String> {
        self.with_env(|env, activity| {
            let object = env
                .call_method(activity, method, jni_sig!("()Ljava/lang/String;"), &[])?
                .l()?;
            if object.is_null() {
                return Ok(None);
            }
            JString::cast_local(env, object)?
                .try_to_string(env)
                .map(Some)
        })
    }

    fn with_env<T>(
        &self,
        callback: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<T>,
    ) -> Result<T, String> {
        // A permanent attachment is a no-op on threads the JVM already owns.
        self.vm
            .attach_current_thread(|env| {
                callback(env, self.activity.as_obj()).inspect_err(|_| {
                    if env.exception_check() {
                        env.exception_describe();
                        env.exception_clear();
                    }
                })
            })
            .map_err(|error: jni::errors::Error| error.to_string())
    }
}

/// Android's own word segmentation: the Java ICU the platform ships.
struct SystemWords(JniObject);

impl tcode_client::words::WordSegmenter for SystemWords {
    fn breaks(&self, text: &str) -> Vec<usize> {
        let units = self.0.with_env(|env, activity| {
            let string = env.new_string(text)?;
            let array = env
                .call_method(
                    activity,
                    jni_str!("gpuiWordBreaks"),
                    jni_sig!("(Ljava/lang/String;)[I"),
                    &[JValue::Object(&string)],
                )?
                .l()?;
            let array = JIntArray::cast_local(env, array)?;
            let mut units = vec![0; array.len(env)?];
            array.get_region(env, 0, &mut units)?;
            Ok(units)
        });
        match units {
            Ok(units) => tcode_client::words::breaks_from_utf16(
                text,
                units.into_iter().map(|unit| unit.max(0) as usize),
            ),
            Err(error) => {
                log::warn!("Android word breaks unavailable: {error}");
                tcode_client::words::breaks_from_utf16(text, [])
            }
        }
    }
}

#[derive(Clone)]
struct JavaBridge {
    app: AndroidApp,
    object: JniObject,
}

impl JavaBridge {
    fn new(app: AndroidApp) -> Result<Self, String> {
        // SAFETY: Android owns the VM and activity for the NativeActivity process lifetime.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity = vm
            .attach_current_thread(|env| {
                // SAFETY: `activity_as_ptr` is the live GpuiActivity reference.
                let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
                env.new_global_ref(&activity)
            })
            .map_err(|error: jni::errors::Error| {
                format!("failed retaining GpuiActivity: {error}")
            })?;
        Ok(Self {
            app,
            object: JniObject {
                vm,
                activity: Arc::new(activity),
            },
        })
    }

    fn set_app_background_dark(&self, dark: bool) {
        let object = self.object.clone();
        self.app.run_on_java_main_thread(Box::new(move || {
            if let Err(error) = object.with_env(|env, activity| {
                env.call_method(
                    activity,
                    jni_str!("gpuiSetAppBackgroundDark"),
                    jni_sig!("(Z)V"),
                    &[JValue::Bool(dark)],
                )?;
                Ok(())
            }) {
                log::error!("Android system bar appearance JNI call failed: {error}");
            }
        }));
    }

    fn pick_images(&self, request_id: u64, limit: usize) {
        let object = self.object.clone();
        self.app.run_on_java_main_thread(Box::new(move || {
            if let Err(error) = object.with_env(|env, activity| {
                env.call_method(
                    activity,
                    jni_str!("gpuiPickImages"),
                    jni_sig!("(JI)V"),
                    &[
                        JValue::Long(request_id as i64),
                        JValue::Int(i32::try_from(limit).unwrap_or(i32::MAX)),
                    ],
                )?;
                Ok(())
            }) {
                log::error!("Android image picker JNI call failed: {error}");
                deliver_images_picked(request_id, 2, Some(error));
            }
        }));
    }

    fn start_camera(&self, request_id: u64) {
        let object = self.object.clone();
        self.app.run_on_java_main_thread(Box::new(move || {
            if let Err(error) = object.with_env(|env, activity| {
                env.call_method(
                    activity,
                    jni_str!("gpuiStartCameraScan"),
                    jni_sig!("(J)V"),
                    &[JValue::Long(request_id as i64)],
                )?;
                Ok(())
            }) {
                log::error!("Android camera JNI call failed: {error}");
                deliver_result(request_id, 2, Some(error));
            }
        }));
    }
}

pub(crate) fn native_host(
    app: AndroidApp,
    cx: &mut App,
) -> Result<(Rc<NativeClientHost>, Option<String>), String> {
    let bridge = JavaBridge::new(app)?;
    let appearance_bridge = bridge.clone();
    // Keep system chrome in sync with explicit app themes as well as system mode.
    cx.observe_global::<tcode_ui::theme::Theme>(move |cx| {
        appearance_bridge
            .set_app_background_dark(cx.global::<tcode_ui::theme::Theme>().mode.is_dark());
    })
    .detach();
    let data_dir = bridge
        .object
        .call_string(jni_str!("gpuiDataDir"))?
        .map(PathBuf::from)
        .ok_or_else(|| "Android filesDir is unavailable".to_string())?;
    let device_name = bridge
        .object
        .call_string(jni_str!("gpuiDeviceModel"))?
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Android".into());
    let platform = bridge
        .object
        .call_string(jni_str!("gpuiDevicePlatform"))?
        .filter(|platform| !platform.trim().is_empty())
        .unwrap_or_else(|| "Android".into());
    let system_locale = bridge
        .object
        .call_string(jni_str!("gpuiSystemLocale"))?
        .filter(|locale| !locale.trim().is_empty());

    let callbacks = Rc::new(RefCell::new(HashMap::<
        u64,
        async_channel::Sender<Result<String, String>>,
    >::new()));
    let pending = callbacks.clone();
    let picks = Rc::new(RefCell::new(HashMap::<u64, PickRequest>::new()));
    let pending_picks = picks.clone();
    let multicast = bridge.object.clone();
    let camera = bridge.clone();
    let picker = bridge.clone();
    let host = NativeClientHost::new(data_dir, device_name)
        .with_platform(platform)
        .with_word_segmenter(SystemWords(bridge.object.clone()))
        .with_multicast_lock(move |acquire| {
            if let Err(error) = multicast.with_env(|env, activity| {
                env.call_method(
                    activity,
                    jni_str!("gpuiMulticastLock"),
                    jni_sig!("(Z)V"),
                    &[JValue::Bool(acquire)],
                )?;
                Ok(())
            }) {
                log::warn!("Android multicast lock unavailable: {error}");
            }
        })
        .with_qr_scanner(move || -> HostFuture<'static, Result<String, String>> {
            let (sender, receiver) = async_channel::bounded(1);
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            callbacks.borrow_mut().insert(request_id, sender);
            camera.start_camera(request_id);
            Box::pin(async move {
                receiver
                    .recv()
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            })
        })
        .with_image_picker(
            move |limit| -> HostFuture<'static, Result<Vec<PickedImage>, String>> {
                let (done, receiver) = async_channel::bounded(1);
                let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
                picks.borrow_mut().insert(
                    request_id,
                    PickRequest {
                        images: Vec::new(),
                        done,
                    },
                );
                picker.pick_images(request_id, limit);
                Box::pin(async move {
                    receiver
                        .recv()
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()))
                })
            },
        );
    let host = Rc::new(host);

    let (sender, mut receiver) = mpsc::unbounded();
    *EVENT_SENDER.lock().expect("Android event sender poisoned") = Some(sender);
    let events_host = host.clone();
    cx.spawn(async move |cx| {
        while let Some(event) = receiver.next().await {
            let pending = pending.clone();
            let picks = pending_picks.clone();
            let host = events_host.clone();
            cx.update(move |_cx| match event {
                BridgeEvent::NetworkChanged => host.network_changed(),
                BridgeEvent::ImagePicked { request_id, image } => {
                    if let Some(request) = picks.borrow_mut().get_mut(&request_id) {
                        request.images.push(image);
                    } else {
                        log::warn!("image for unknown Android picker request {request_id}");
                    }
                }
                BridgeEvent::ImagesPicked {
                    request_id,
                    status,
                    error,
                } => {
                    let request = picks.borrow_mut().remove(&request_id);
                    let Some(request) = request else {
                        log::warn!("result for unknown Android picker request {request_id}");
                        return;
                    };
                    let result = match status {
                        RESULT_OK | RESULT_CANCELLED => Ok(request.images),
                        _ => Err(error.unwrap_or_else(|| "Android image picker failed".into())),
                    };
                    let _ = request.done.try_send(result);
                }
                BridgeEvent::CameraResult {
                    request_id,
                    status,
                    value,
                } => {
                    let sender = pending.borrow_mut().remove(&request_id);
                    let Some(sender) = sender else {
                        log::warn!(
                            "received result for unknown Android camera request {request_id}"
                        );
                        return;
                    };
                    let result = match (status, value) {
                        (RESULT_OK, Some(value)) if !value.is_empty() => Ok(value),
                        (RESULT_CANCELLED, value) => {
                            Err(value.unwrap_or_else(|| "已取消扫描".into()))
                        }
                        (_, value) => Err(value.unwrap_or_else(|| "Android 相机扫描失败".into())),
                    };
                    let _ = sender.try_send(result);
                }
            });
        }
    })
    .detach();
    Ok((host, system_locale))
}

fn send_event(event: BridgeEvent, dropped: &str) {
    let sender = EVENT_SENDER
        .lock()
        .expect("Android event sender poisoned")
        .clone();
    if let Some(sender) = sender {
        let _ = sender.unbounded_send(event);
    } else {
        log::warn!("dropping Android {dropped} before host initialization");
    }
}

pub(crate) fn deliver_image_picked(request_id: u64, image: PickedImage) {
    send_event(
        BridgeEvent::ImagePicked { request_id, image },
        "picked image",
    );
}

pub(crate) fn deliver_images_picked(request_id: u64, status: i32, error: Option<String>) {
    send_event(
        BridgeEvent::ImagesPicked {
            request_id,
            status,
            error,
        },
        "image picker result",
    );
}

pub(crate) fn deliver_result(request_id: u64, status: i32, value: Option<String>) {
    send_event(
        BridgeEvent::CameraResult {
            request_id,
            status,
            value,
        },
        "camera result",
    );
}

/// The activity's `ConnectivityManager.NetworkCallback` fired or the
/// activity resumed: the device endpoint rebinds its paths and every live
/// transport is probed at once ([`NativeClientHost::network_changed`]).
///
/// The endpoint is not shut down when the activity stops. iroh's Android
/// guidance is to close it before backgrounding and rebind on return, but
/// the reconnecting transport already notices a dead connection through
/// `closed()`, the shell's foreground policy reconnects outright after ten
/// seconds away, and this call arrives on every resume. A socket the OS
/// destroyed in the background is only rebound when iroh's interface watcher
/// classifies the resume as a major link change; whether Android leaves the
/// interface state unchanged in that case has not been observed on a device.
/// TODO(traverse-android-background): if a resumed app keeps failing to
/// reconnect until the network actually changes, close the endpoint on
/// `Background` and rebuild it on `Foreground` instead of relying on rebind.
pub(crate) fn network_changed() {
    send_event(BridgeEvent::NetworkChanged, "network change");
}
