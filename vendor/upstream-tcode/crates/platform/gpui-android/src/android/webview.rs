//! JNI transport for activity-owned browser children. No GPUI entities cross threads.
use futures::channel::{mpsc, oneshot};
use jni::{
    EnvUnowned, JavaVM,
    errors::LogErrorAndDefault,
    jni_sig, jni_str,
    objects::{JByteArray, JObject, JString, JValue},
    sys::{jint, jlong},
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(1);
static VIEWS: LazyLock<Mutex<HashMap<u64, mpsc::UnboundedSender<Event>>>> =
    LazyLock::new(Default::default);
static REQUESTS: LazyLock<Mutex<HashMap<u64, Pending>>> = LazyLock::new(Default::default);
struct Pending {
    view: u64,
    sender: oneshot::Sender<Result<Reply, String>>,
}

#[derive(Debug)]
pub struct Event {
    pub kind: i32,
    pub url: String,
    pub title: String,
    pub code: i32,
    pub message: String,
}

pub enum Reply {
    Json(String),
    Png(Vec<u8>),
}

/// A generation id is never reused, including across activity recreation.
pub struct Browser {
    id: u64,
}
impl Browser {
    pub fn new(initial_url: &str) -> Result<(Self, mpsc::UnboundedReceiver<Event>), String> {
        if super::host::APP.lock().is_none() {
            return Err("Android activity is unavailable".into());
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::unbounded();
        VIEWS.lock().insert(id, sender);
        let browser = Self { id };
        browser.command("create", initial_url, [0; 4]);
        Ok((browser, receiver))
    }
    pub fn command(&self, operation: &str, value: &str, bounds: [i32; 4]) {
        dispatch(self.id, 0, operation.to_owned(), value.to_owned(), bounds);
    }
    pub fn request(
        &self,
        operation: &str,
        value: &str,
    ) -> impl Future<Output = Result<Reply, String>> + use<> {
        let request = NEXT.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        REQUESTS.lock().insert(
            request,
            Pending {
                view: self.id,
                sender,
            },
        );
        dispatch(
            self.id,
            request,
            operation.to_owned(),
            value.to_owned(),
            [0; 4],
        );
        let guard = RequestGuard(request);
        async move {
            let _guard = guard;
            receiver
                .await
                .map_err(|_| "preview request cancelled".to_string())?
        }
    }
}
struct RequestGuard(u64);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        REQUESTS.lock().remove(&self.0);
    }
}
impl Drop for Browser {
    fn drop(&mut self) {
        VIEWS.lock().remove(&self.id);
        REQUESTS.lock().retain(|_, request| request.view != self.id);
        self.command("destroy", "", [0; 4]);
    }
}

fn failed(view: u64, request: u64, message: String) {
    if request != 0 {
        complete(request, Err(message));
    } else if let Some(sender) = VIEWS.lock().get(&view) {
        let _ = sender.unbounded_send(Event {
            kind: 6,
            url: String::new(),
            title: String::new(),
            code: 0,
            message,
        });
    }
}
fn complete(request: u64, result: Result<Reply, String>) {
    if let Some(pending) = REQUESTS.lock().remove(&request) {
        let _ = pending.sender.send(result);
    }
}
fn dispatch(id: u64, request: u64, operation: String, value: String, bounds: [i32; 4]) {
    let Some(app) = super::host::APP.lock().clone() else {
        failed(id, request, "Android activity is unavailable".into());
        return;
    };
    let callback_app = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        // SAFETY: the AndroidApp retains the activity and its VM for this callback.
        let vm = unsafe { JavaVM::from_raw(callback_app.vm_as_ptr().cast()) };
        // The UI thread is already attached, so this only pushes a local frame.
        let result = vm
            .attach_current_thread(|env| -> jni::errors::Result<()> {
                // SAFETY: this is the live NativeActivity instance; do not delete its reference.
                let activity =
                    unsafe { JObject::from_raw(env, callback_app.activity_as_ptr().cast()) };
                let host = env
                    .get_field(
                        &activity,
                        jni_str!("previewHost"),
                        jni_sig!("Lcom/tryanks/tcode/PreviewHost;"),
                    )?
                    .l()?;
                let operation = env.new_string(&operation)?;
                let value = env.new_string(&value)?;
                env.call_method(
                    host,
                    jni_str!("command"),
                    jni_sig!("(JJLjava/lang/String;Ljava/lang/String;IIII)V"),
                    &[
                        JValue::Long(id as i64),
                        JValue::Long(request as i64),
                        JValue::Object(operation.as_ref()),
                        JValue::Object(value.as_ref()),
                        JValue::Int(bounds[0]),
                        JValue::Int(bounds[1]),
                        JValue::Int(bounds[2]),
                        JValue::Int(bounds[3]),
                    ],
                )
                .inspect_err(|_| env.exception_clear())?;
                Ok(())
            })
            .map_err(|e| e.to_string());
        if let Err(error) = result {
            failed(id, request, error);
        }
    }));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_PreviewHost_nativeResult<'local>(
    mut env: EnvUnowned<'local>,
    _class: JObject<'local>,
    request: jlong,
    value: JString<'local>,
    png: JByteArray<'local>,
    error: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let result = if !error.is_null() {
            Err(error.try_to_string(env).unwrap_or_else(|e| e.to_string()))
        } else if !png.is_null() {
            env.convert_byte_array(&png)
                .map(Reply::Png)
                .map_err(|e| e.to_string())
        } else {
            value
                .try_to_string(env)
                .map(Reply::Json)
                .map_err(|e| e.to_string())
        };
        complete(request as u64, result);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_PreviewHost_nativeEvent<'local>(
    mut env: EnvUnowned<'local>,
    _class: JObject<'local>,
    id: jlong,
    kind: jint,
    url: JString<'local>,
    title: JString<'local>,
    code: jint,
    message: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let string = |value: &JString| value.try_to_string(env).unwrap_or_default();
        let event = Event {
            kind,
            url: string(&url),
            title: string(&title),
            code,
            message: string(&message),
        };
        if let Some(sender) = VIEWS.lock().get(&(id as u64)) {
            let _ = sender.unbounded_send(event);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}
