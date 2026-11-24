//! Shared authenticated HTTP host for tcode's in-process MCP tool servers.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use rmcp::ServerHandler;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

#[derive(Clone, Copy)]
pub struct BrokerErrors {
    pub unavailable: &'static str,
    pub dropped: &'static str,
    pub timed_out: &'static str,
}

pub struct Broker<Request> {
    requests: async_channel::Sender<Request>,
    timeout: Duration,
    errors: BrokerErrors,
}

impl<Request> Clone for Broker<Request> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            timeout: self.timeout,
            errors: self.errors,
        }
    }
}

impl<Request> Broker<Request> {
    pub fn new(
        requests: async_channel::Sender<Request>,
        timeout: Duration,
        errors: BrokerErrors,
    ) -> Self {
        Self {
            requests,
            timeout,
            errors,
        }
    }

    pub async fn invoke<Reply>(
        &self,
        request: impl FnOnce(async_channel::Sender<Result<Reply, String>>) -> Request,
    ) -> Result<Reply, String> {
        let (reply, response) = async_channel::bounded(1);
        self.requests
            .send(request(reply))
            .await
            .map_err(|_| self.errors.unavailable.to_string())?;
        match tokio::time::timeout(self.timeout, response.recv()).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.errors.dropped.to_string()),
            Err(_) => Err(self.errors.timed_out.to_string()),
        }
    }
}

pub struct TokenRegistry<Service> {
    services: Arc<RwLock<HashMap<String, Service>>>,
    create: Arc<dyn Fn(String) -> Service + Send + Sync>,
}

impl<Service> Clone for TokenRegistry<Service> {
    fn clone(&self) -> Self {
        Self {
            services: self.services.clone(),
            create: self.create.clone(),
        }
    }
}

impl<Service> TokenRegistry<Service> {
    pub fn new(create: impl Fn(String) -> Service + Send + Sync + 'static) -> Self {
        Self {
            services: Arc::new(RwLock::new(HashMap::new())),
            create: Arc::new(create),
        }
    }

    pub fn register(&self, scope: &str) -> String {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let service = (self.create)(scope.to_string());
        self.services
            .write()
            .unwrap()
            .insert(token.clone(), service);
        token
    }

    pub fn revoke(&self, token: &str) {
        self.services.write().unwrap().remove(token);
    }
}

pub struct Route(Router);

pub fn route<Handler>(
    path: &str,
    tokens: &TokenRegistry<StreamableHttpService<Handler, LocalSessionManager>>,
) -> Route
where
    Handler: ServerHandler + Clone + Send + Sync + 'static,
{
    let services = tokens.services.clone();
    let handler =
        move |request: axum::extract::Request| handle_authenticated(services.clone(), request);
    Route(Router::new().route(path, any(handler)))
}

async fn handle_authenticated<Handler>(
    services: Arc<RwLock<HashMap<String, StreamableHttpService<Handler, LocalSessionManager>>>>,
    request: axum::extract::Request,
) -> Response
where
    Handler: ServerHandler + Clone + Send + Sync + 'static,
{
    let token = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let service = token.and_then(|token| services.read().unwrap().get(token).cloned());
    let Some(service) = service else {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };
    let response = service.handle(request).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, axum::body::Body::new(body))
}

pub struct Host {
    listener: std::net::TcpListener,
    base_url: String,
    app: Router,
}

impl Host {
    pub fn bind() -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            base_url: format!("http://127.0.0.1:{port}"),
            app: Router::new(),
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub fn mount(&mut self, route: Route) {
        self.app = std::mem::take(&mut self.app).merge(route.0);
    }

    pub fn start(self) -> std::io::Result<()> {
        self.listener.set_nonblocking(true)?;
        std::thread::Builder::new()
            .name("mcp-host".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        log::error!("mcp-host: failed to build tokio runtime: {error}");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let listener = match tokio::net::TcpListener::from_std(self.listener) {
                        Ok(listener) => listener,
                        Err(error) => {
                            log::error!("mcp-host: failed to adopt listener: {error}");
                            return;
                        }
                    };
                    if let Err(error) = axum::serve(listener, self.app).await {
                        log::error!("mcp-host: server exited with error: {error}");
                    }
                });
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_keep_scopes_isolated_and_revoke_independently() {
        let registry = TokenRegistry::new(|scope| scope);
        let first = registry.register("session-a");
        let second = registry.register("session-b");
        assert_ne!(first, second);
        assert_eq!(registry.services.read().unwrap()[&first], "session-a");
        registry.revoke(&first);
        let services = registry.services.read().unwrap();
        assert!(!services.contains_key(&first));
        assert_eq!(services[&second], "session-b");
    }
}
