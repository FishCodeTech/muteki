//! A single browser task owns the socket and pending writes. HostLink restores
//! subscriptions from applied cursors. Every browser callback only enqueues
//! an event; dropping a connection removes callbacks,
//! event listeners and timers so stale sockets cannot affect a newer attempt.
use std::collections::VecDeque;

use async_channel::{Receiver, Sender};
use futures_lite::future::race;
use tcode_client::{
    ConnectionFailure, ConnectionState,
    heartbeat::{Heartbeat, Tick},
    host::{DeviceIdentity, Transport},
    outgoing::{OutgoingReceiver, subscription_key},
    recovery::Backoff,
};
use wasm_bindgen::{JsCast as _, prelude::*};

use crate::host::window;

enum Event {
    Open,
    Text(String),
    Lost,
    Wake,
    Timeout,
}

struct Listener {
    target: web_sys::EventTarget,
    name: &'static str,
    callback: Closure<dyn FnMut(web_sys::Event)>,
}

impl Listener {
    fn new(
        target: &web_sys::EventTarget,
        name: &'static str,
        f: impl FnMut(web_sys::Event) + 'static,
    ) -> Self {
        let callback = Closure::wrap(Box::new(f) as Box<dyn FnMut(_)>);
        target
            .add_event_listener_with_callback(name, callback.as_ref().unchecked_ref())
            .expect("browser event listener");
        Self {
            target: target.clone(),
            name,
            callback,
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self
            .target
            .remove_event_listener_with_callback(self.name, self.callback.as_ref().unchecked_ref());
    }
}

struct Timer {
    id: i32,
    _callback: Closure<dyn FnMut()>,
}

impl Timer {
    fn new(milliseconds: i32, tx: Sender<Event>) -> Self {
        let callback = Closure::wrap(Box::new(move || {
            let _ = tx.try_send(Event::Timeout);
        }) as Box<dyn FnMut()>);
        let id = window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                milliseconds,
            )
            .expect("browser timeout");
        Self {
            id,
            _callback: callback,
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        window().clear_timeout_with_handle(self.id);
    }
}

struct Socket {
    ws: web_sys::WebSocket,
    _listeners: Vec<Listener>,
}

impl Socket {
    fn new(tx: &Sender<Event>) -> Result<Self, JsValue> {
        let location = window().location();
        let scheme = if location.protocol()? == "https:" {
            "wss"
        } else {
            "ws"
        };
        let ws = web_sys::WebSocket::new(&format!("{scheme}://{}/ws", location.host()?))?;
        let mut listeners = Vec::new();
        for (name, kind) in [("open", 0), ("message", 1), ("close", 2), ("error", 2)] {
            let tx = tx.clone();
            listeners.push(Listener::new(ws.as_ref(), name, move |event| {
                let event = match kind {
                    0 => Event::Open,
                    1 => match event
                        .dyn_into::<web_sys::MessageEvent>()
                        .ok()
                        .and_then(|event| event.data().as_string())
                    {
                        Some(text) => Event::Text(text),
                        None => Event::Lost,
                    },
                    _ => Event::Lost,
                };
                let _ = tx.try_send(event);
            }));
        }
        Ok(Self {
            ws,
            _listeners: listeners,
        })
    }

    fn send(&self, line: &str) -> Result<(), JsValue> {
        if self.ws.ready_state() != web_sys::WebSocket::OPEN {
            return Err(JsValue::from_str("socket is not open"));
        }
        self.ws.send_with_str(line.trim_end())
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self._listeners.clear();
        let _ = self.ws.close();
    }
}

pub fn connect(token: String, device: DeviceIdentity) -> Transport {
    let (to_host, outgoing) = tcode_client::outgoing::channel();
    let (incoming, from_host) = async_channel::unbounded();
    let (state_tx, state) = async_channel::unbounded();
    let originals = std::sync::Arc::new(BrowserOriginal(token.clone()));
    wasm_bindgen_futures::spawn_local(async move {
        // Receiver-only closure must also wake an idle connection/backoff.
        race(
            connection_loop(token, device, &outgoing, &incoming, &state_tx),
            async {
                race(incoming.closed(), state_tx.closed()).await;
            },
        )
        .await;

        incoming.close();
        outgoing.close();
    });
    Transport {
        originals: Some(originals),
        to_host,
        from_host,
        state,
        current_host: None,
    }
}

struct BrowserOriginal(String);
struct AbortOriginal(web_sys::AbortController);
impl Drop for AbortOriginal {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl tcode_client::original::OriginalOpener for BrowserOriginal {
    fn open(
        &self,
        target: tcode_protocol::OriginalTarget,
    ) -> tcode_client::original::OriginalFuture<'static, tcode_client::original::OpenedOriginal>
    {
        let token = self.0.clone();
        Box::pin(async move {
            use tcode_client::original::{OpenedOriginal, error};
            let abort = AbortOriginal(web_sys::AbortController::new().map_err(js_error)?);
            let init = web_sys::RequestInit::new();
            init.set_method("GET");
            init.set_signal(Some(&abort.0.signal()));
            let target = serde_json::to_string(&target).map_err(error)?;
            let target = js_sys::encode_uri_component(&target)
                .as_string()
                .ok_or_else(|| error("invalid target"))?;
            let request = web_sys::Request::new_with_str_and_init(
                &format!("/original?target={target}"),
                &init,
            )
            .map_err(js_error)?;
            request
                .headers()
                .set("Authorization", &format!("Bearer {token}"))
                .map_err(js_error)?;
            let response: web_sys::Response =
                original_await(window().fetch_with_request(&request), &abort.0)
                    .await?
                    .dyn_into()
                    .map_err(js_error)?;
            if response.status() != 200 {
                return Err(tcode_protocol::ProtocolError {
                    code: match response.status() {
                        401 | 403 => "out_of_scope",
                        404 => "not_found",
                        409 => "changed",
                        429 => "busy",
                        _ => "io",
                    }
                    .into(),
                    message: format!("original request failed ({})", response.status()),
                });
            }
            let len = response
                .headers()
                .get("Content-Length")
                .map_err(js_error)?
                .ok_or_else(|| error("original has no length"))?
                .parse::<u64>()
                .map_err(error)?;
            let mime = response.headers().get("Content-Type").map_err(js_error)?;
            let dimensions = response
                .headers()
                .get("X-Image-Dimensions")
                .map_err(js_error)?
                .and_then(|value| {
                    let (width, height) = value.split_once('x')?;
                    Some((width.parse().ok()?, height.parse().ok()?))
                });
            let reader: web_sys::ReadableStreamDefaultReader = response
                .body()
                .ok_or_else(|| error("original has no body"))?
                .get_reader()
                .dyn_into()
                .map_err(js_error)?;
            Ok(OpenedOriginal {
                metadata: tcode_protocol::OriginalMetadata {
                    len,
                    mime,
                    dimensions,
                },
                reader: Box::new(BrowserOriginalReader {
                    reader,
                    abort,
                    pending: None,
                    offset: 0,
                    remaining: len,
                }),
            })
        })
    }
}

struct BrowserOriginalReader {
    reader: web_sys::ReadableStreamDefaultReader,
    abort: AbortOriginal,
    pending: Option<js_sys::Uint8Array>,
    offset: u32,
    remaining: u64,
}
impl tcode_client::original::OriginalReader for BrowserOriginalReader {
    fn read(&mut self) -> tcode_client::original::OriginalFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            use tcode_client::original::error;
            loop {
                if let Some(pending) = &self.pending {
                    let end = (self.offset + 64 * 1024).min(pending.length());
                    let bytes = pending.subarray(self.offset, end).to_vec();
                    self.offset = end;
                    if end == pending.length() {
                        self.pending = None;
                        self.offset = 0;
                    }
                    if bytes.len() as u64 > self.remaining {
                        return Err(error("original exceeded its length"));
                    }
                    self.remaining -= bytes.len() as u64;
                    if !bytes.is_empty() {
                        return Ok(Some(bytes));
                    }
                }
                let result = original_await(self.reader.read(), &self.abort.0).await?;
                if js_sys::Reflect::get(&result, &JsValue::from_str("done"))
                    .map_err(js_error)?
                    .as_bool()
                    == Some(true)
                {
                    return if self.remaining == 0 {
                        Ok(None)
                    } else {
                        Err(error("original ended short"))
                    };
                }
                let value =
                    js_sys::Reflect::get(&result, &JsValue::from_str("value")).map_err(js_error)?;
                self.pending = Some(value.dyn_into().map_err(js_error)?);
            }
        })
    }
    fn cancel(&mut self) {
        self.abort.0.abort();
        self.pending = None;
    }
}

fn js_error(value: impl Into<JsValue>) -> tcode_protocol::ProtocolError {
    let value = value.into();
    tcode_client::original::error(value.as_string().unwrap_or_else(|| format!("{value:?}")))
}

async fn original_await(
    promise: js_sys::Promise,
    abort: &web_sys::AbortController,
) -> Result<JsValue, tcode_protocol::ProtocolError> {
    let (tx, elapsed) = async_channel::bounded(1);
    let _timer = Timer::new(60_000, tx);
    race(
        async {
            wasm_bindgen_futures::JsFuture::from(promise)
                .await
                .map_err(js_error)
        },
        async {
            let _ = elapsed.recv().await;
            abort.abort();
            Err(tcode_client::original::error(
                "original made no progress for 60 seconds",
            ))
        },
    )
    .await
}

enum Input {
    Line(Result<String, async_channel::RecvError>),
    Event(Event),
}

async fn next(outgoing: &OutgoingReceiver, events: &Receiver<Event>) -> Input {
    race(async { Input::Line(outgoing.recv().await) }, async {
        Input::Event(events.recv().await.unwrap_or(Event::Lost))
    })
    .await
}

async fn connection_loop(
    token: String,
    device: DeviceIdentity,
    outgoing: &OutgoingReceiver,
    incoming: &Sender<String>,
    state: &Sender<ConnectionState>,
) {
    let mut buffered = VecDeque::<String>::new();
    let mut backoff = Backoff::default();
    let mut delay = 0;
    let mut reason = None;
    loop {
        if outgoing.is_closed() {
            return;
        }
        outgoing.discard_retained_writes(&mut buffered);
        let _ = state.try_send(ConnectionState::Reconnecting {
            attempt: backoff.attempt(),
            reason,
        });
        // Each attempt has its own event queue, including foreground wakeups.
        let (tx, events) = async_channel::unbounded();
        let wake_tx = tx.clone();
        let _online = Listener::new(window().as_ref(), "online", move |_| {
            let _ = wake_tx.try_send(Event::Wake);
        });
        let wake_tx = tx.clone();
        let document = window().document().expect("browser document");
        let _visible = Listener::new(document.as_ref(), "visibilitychange", move |_| {
            if !window().document().unwrap().hidden() {
                let _ = wake_tx.try_send(Event::Wake);
            }
        });
        if delay > 0 {
            let _timer = Timer::new(delay, tx.clone());
            loop {
                match next(outgoing, &events).await {
                    Input::Line(Ok(line)) => buffer_write(line, &mut buffered),
                    Input::Line(Err(_)) => return,
                    Input::Event(Event::Timeout | Event::Wake) => break,
                    _ => {}
                }
            }
        }
        // Discard any backoff timer/wakeup already queued before opening.
        while events.try_recv().is_ok() {}
        let mut immediate = false;
        let mut stable_ms = 0;
        reason = Some(ConnectionFailure::Unreachable(None));
        if let Ok(socket) = Socket::new(&tx) {
            let mut timer = Some(Timer::new(15000, tx.clone()));
            let mut ready = false;
            let mut ready_since = None;
            let mut connected = false;
            let mut heartbeat = Heartbeat::new(now_ms());
            loop {
                match next(outgoing, &events).await {
                    Input::Line(Err(_)) => return,
                    Input::Line(Ok(line)) => {
                        if !ready || socket.send(&line).is_err() {
                            if subscription_key(&line).is_none() {
                                buffered.push_back(line);
                            }
                            if ready {
                                break;
                            }
                        } else {
                            outgoing.sent(&line);
                        }
                    }
                    Input::Event(Event::Open) => {
                        if socket.send(&browser_hello(&device, &token)).is_err() {
                            break;
                        }
                    }
                    Input::Event(Event::Text(line)) if !ready => {
                        let hello: serde_json::Value =
                            serde_json::from_str(&line).unwrap_or_default();
                        let failure = if hello["type"].as_str() == Some("hello_rejected") {
                            Some(ConnectionFailure::hello_rejected(hello["reason"].as_str()))
                        } else if hello["type"].as_str() == Some("hello_ok")
                            && hello["protocol_version"].as_u64()
                                != Some(u64::from(tcode_protocol::PROTOCOL_VERSION))
                        {
                            Some(ConnectionFailure::ProtocolMismatch)
                        } else {
                            None
                        };
                        if let Some(failure) = failure {
                            // A revoked or stale token: forget the machine so
                            // the login page takes over on reload.
                            if failure == ConnectionFailure::AuthenticationRejected {
                                if let Ok(Some(storage)) = crate::host::window().local_storage() {
                                    let _ = storage.remove_item("tcode.last_host");
                                }
                                let _ = crate::host::window().location().reload();
                            }
                            if failure.is_terminal() {
                                let _ =
                                    state.try_send(ConnectionState::Offline { reason: failure });
                                return;
                            }
                            reason = Some(failure);
                            break;
                        }
                        if hello["type"].as_str() != Some("hello_ok") {
                            break;
                        }
                        let _ = state.try_send(ConnectionState::Syncing { path: None });
                        timer.take();
                        let mut failed = false;
                        while let Some(line) = buffered.front() {
                            if socket.send(line).is_err() {
                                failed = true;
                                break;
                            }
                            outgoing.sent(line);
                            buffered.pop_front();
                        }
                        if failed {
                            break;
                        }
                        ready = true;
                        ready_since = Some(now_ms());
                        heartbeat.received(now_ms());
                        timer = Some(Timer::new(15000, tx.clone()));
                    }
                    Input::Event(Event::Text(line)) => {
                        heartbeat.received(now_ms());
                        timer = Some(Timer::new(15000, tx.clone()));
                        if !connected {
                            connected = true;
                            let _ = state.try_send(ConnectionState::Connected { path: None });
                        }
                        if incoming.try_send(format!("{}\n", line.trim_end())).is_err() {
                            return;
                        }
                    }
                    Input::Event(Event::Wake) => {
                        immediate = true;
                        break;
                    }
                    Input::Event(Event::Timeout) if ready => {
                        match heartbeat.tick(now_ms()) {
                            Tick::Wait(ms) => timer = Some(Timer::new(ms as i32, tx.clone())),
                            Tick::Ping => {
                                // ID zero is reserved for transport probes; HostLink starts at one.
                                let ping = tcode_protocol::ClientMessage {
                                    principal: None,
                                    key: None,
                                    id: 0,
                                    payload: tcode_protocol::ClientPayload::Query(
                                        tcode_protocol::Query::Ping,
                                    ),
                                };
                                if socket
                                    .send(
                                        &serde_json::to_string(&ping)
                                            .expect("heartbeat serialization"),
                                    )
                                    .is_err()
                                {
                                    break;
                                }
                                timer = Some(Timer::new(
                                    tcode_client::heartbeat::LIVENESS_REPLY_MS as i32,
                                    tx.clone(),
                                ));
                            }
                            Tick::Lost => {
                                reason = Some(ConnectionFailure::Timeout);
                                break;
                            }
                        }
                    }
                    Input::Event(Event::Timeout) => {
                        reason = Some(ConnectionFailure::Timeout);
                        break;
                    }
                    Input::Event(Event::Lost) => {
                        reason = Some(ConnectionFailure::HostClosed);
                        break;
                    }
                }
            }
            if connected {
                stable_ms = ready_since.map_or(0, |start| now_ms().saturating_sub(start));
            }
        }
        delay = if immediate {
            0
        } else {
            backoff.failed(stable_ms, js_sys::Math::random()) as i32
        };
    }
}

fn buffer_write(line: String, buffered: &mut VecDeque<String>) {
    if subscription_key(&line).is_none() {
        buffered.push_back(line);
    }
}

/// The browser listener authenticates by the bearer token login issued;
/// the device fields name this browser in the machine's device list.
fn browser_hello(device: &DeviceIdentity, token: &str) -> String {
    serde_json::json!({
        "type": "hello",
        "protocol_version": tcode_protocol::PROTOCOL_VERSION,
        "token": token,
        "device": {"name": device.name, "platform": device.platform},
    })
    .to_string()
}

fn now_ms() -> u64 {
    window()
        .performance()
        .expect("browser performance clock")
        .now() as u64
}
