//! One listener for everything: `GET /relay` is handed to iroh-relay's
//! embeddable `RelayService` (which upgrades the WebSocket itself), every
//! other path goes to the axum router. `RelayService` downcasts the upgraded
//! connection to `TokioIo<MaybeTlsStream>`, so connections are served with
//! hyper directly instead of `axum::serve`.
use std::{
    convert::Infallible, future::Future, net::SocketAddr, pin::Pin, sync::Arc, time::Duration,
};

use axum::{
    Router,
    body::Body,
    http::{HeaderValue, Request, Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use hyper::{body::Incoming, server::conn::http1, service::Service};
use hyper_util::rt::{TokioIo, TokioTimer};
use iroh_relay::{
    http::{RELAY_PATH, RELAY_PROBE_PATH},
    server::{
        RelayService,
        http_server::{HyperError, RelayServiceWithNotify},
        streams::MaybeTlsStream,
    },
};
use tokio::{net::TcpStream, sync::Notify};
use tower::ServiceExt as _;

use crate::{pkarr::PeerAddr, tls::Tls};

/// Bounds the TLS handshake and each request's header read (which, on a
/// keep-alive connection, is also the idle time).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Dispatch {
    relay: Option<RelayService>,
    router: Router,
}

impl Dispatch {
    pub fn new(relay: RelayService, router: Router) -> Self {
        Self {
            relay: Some(relay),
            router,
        }
    }

    /// The same routes with `/relay` answering 404, for the plain listener
    /// beside a TLS one.
    pub fn without_relay(&self) -> Self {
        Self {
            relay: None,
            router: self.router.clone(),
        }
    }
}

impl Service<Request<Incoming>> for Dispatch {
    type Response = Response<Body>;
    type Error = HyperError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        if let Some(relay) = &self.relay
            && request.method() == http::Method::GET
            && request.uri().path() == RELAY_PATH
        {
            let relay = RelayServiceWithNotify::new(relay.clone(), Arc::new(Notify::new()));
            let response = relay.call(request);
            return Box::pin(async move { response.await.map(|response| response.map(Body::new)) });
        }
        let router = self.router.clone();
        Box::pin(async move {
            let response: Result<Response<Body>, Infallible> = router.oneshot(request).await;
            Ok(response.expect("axum router is infallible"))
        })
    }
}

// A named `async fn`: with a closure returning an `async` block rustc infers
// a non-`'static` lifetime for the boxed error and rejects the future.
async fn handle(
    dispatch: Dispatch,
    peer: SocketAddr,
    mut request: Request<Incoming>,
) -> Result<Response<Body>, HyperError> {
    request.extensions_mut().insert(PeerAddr(peer));
    dispatch.call(request).await
}

/// Serves one accepted TCP connection to completion.
pub async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    tls: Option<Arc<Tls>>,
    dispatch: Dispatch,
) {
    let stream = match tls {
        Some(tls) => {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(stream)).await {
                Ok(Ok(Some(stream))) => MaybeTlsStream::Tls(stream),
                // A TLS-ALPN-01 validation, answered inside the acceptor.
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    tracing::debug!(%peer, "tls handshake failed: {error}");
                    return;
                }
                Err(_) => {
                    tracing::debug!(%peer, "tls handshake timed out");
                    return;
                }
            }
        }
        None => MaybeTlsStream::Plain(stream),
    };
    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
        handle(dispatch.clone(), peer, request)
    });
    let result = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HANDSHAKE_TIMEOUT)
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;
    if let Err(error) = result
        && !error.is_incomplete_message()
    {
        tracing::debug!(%peer, "connection ended: {error}");
    }
}

/// The routes every Traverse instance serves besides `/relay` and the pkarr
/// store: manifest, health, and the probes iroh clients send a relay.
pub fn base_router(manifest_json: Arc<String>) -> Router {
    Router::new()
        .route(
            "/relays.json",
            get(move || {
                let manifest = manifest_json.clone();
                async move {
                    (
                        [
                            (header::CONTENT_TYPE, "application/json"),
                            (header::CACHE_CONTROL, "public, max-age=300"),
                            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                        ],
                        manifest.as_str().to_owned(),
                    )
                }
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route(
            RELAY_PROBE_PATH,
            get(|| async { [(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")] }),
        )
        .route("/generate_204", get(no_content))
        .route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "Traverse: an iroh relay and pkarr store for Tcode. See /relays.json.\n",
                )
            }),
        )
        .route(
            "/robots.txt",
            get(|| async { "User-agent: *\nDisallow: /\n" }),
        )
}

/// Captive-portal probe: `X-Iroh-Challenge: <token>` is echoed as
/// `X-Iroh-Response: response <token>` so a client can tell a real relay from
/// a portal page.
async fn no_content(request: Request<Body>) -> Response<Body> {
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Some(challenge) = request.headers().get("x-iroh-challenge")
        && let Ok(challenge) = challenge.to_str()
        && !challenge.is_empty()
        && challenge.len() < 64
        && challenge
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && let Ok(value) = HeaderValue::from_str(&format!("response {challenge}"))
    {
        response.headers_mut().insert("x-iroh-response", value);
    }
    response
}
