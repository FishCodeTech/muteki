//! TLS for the relay listener and, through the same `rustls::ServerConfig`,
//! the QUIC address-discovery socket.
use std::{io, path::PathBuf, sync::Arc};

use futures_lite::StreamExt as _;
use iroh_relay::{
    server::{DEFAULT_CERT_RELOAD_INTERVAL, reloading_resolver},
    tls::CaTlsConfig,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tokio_rustls_acme::{AcmeAcceptor, AcmeConfig, caches::DirCache};

use crate::config::{Config, TlsMode};

pub struct Tls {
    pub server_config: Arc<rustls::ServerConfig>,
    acceptor: Acceptor,
    acme_task: Option<tokio::task::JoinHandle<()>>,
}

enum Acceptor {
    Manual(tokio_rustls::TlsAcceptor),
    Acme(AcmeAcceptor),
}

impl Drop for Tls {
    fn drop(&mut self) {
        if let Some(task) = &self.acme_task {
            task.abort();
        }
    }
}

impl Tls {
    /// `None` when `tls.mode = "off"`.
    pub async fn load(config: &Config) -> io::Result<Option<Self>> {
        let provider = iroh_relay::tls::default_provider();
        let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_no_client_auth();
        let tls = match config.tls.mode {
            TlsMode::Off => return Ok(None),
            TlsMode::Manual => {
                let certs = CertificateDer::pem_file_iter(&config.tls.cert)
                    .map_err(io::Error::other)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(io::Error::other)?;
                let key =
                    PrivateKeyDer::from_pem_file(&config.tls.key).map_err(io::Error::other)?;
                let server_config = Arc::new(
                    builder
                        .with_single_cert(certs, key)
                        .map_err(io::Error::other)?,
                );
                Self {
                    acceptor: Acceptor::Manual(tokio_rustls::TlsAcceptor::from(
                        server_config.clone(),
                    )),
                    server_config,
                    acme_task: None,
                }
            }
            TlsMode::Reloading => {
                let resolver = reloading_resolver(
                    &provider,
                    PathBuf::from(&config.tls.cert),
                    PathBuf::from(&config.tls.key),
                    DEFAULT_CERT_RELOAD_INTERVAL,
                )
                .await
                .map_err(io::Error::other)?;
                let server_config = Arc::new(builder.with_cert_resolver(resolver));
                Self {
                    acceptor: Acceptor::Manual(tokio_rustls::TlsAcceptor::from(
                        server_config.clone(),
                    )),
                    server_config,
                    acme_task: None,
                }
            }
            TlsMode::LetsEncrypt => {
                let hostname = config.hostname.clone().unwrap_or_default();
                let contact = config.contact.clone().unwrap_or_default();
                let client_config = CaTlsConfig::default().client_config(provider)?;
                let mut acme =
                    AcmeConfig::new_with_client_tls_config([hostname], Arc::new(client_config))
                        .contact([format!("mailto:{contact}")])
                        .directory_lets_encrypt(true)
                        .cache(DirCache::new(config.data_dir.join("acme")));
                if let Some(directory) = &config.tls.acme_directory {
                    acme = acme.directory(directory);
                }
                let mut state = acme.state();
                let acceptor = state.acceptor();
                let server_config = Arc::new(builder.with_cert_resolver(state.resolver()));
                let acme_task = tokio::spawn(async move {
                    while let Some(event) = state.next().await {
                        match event {
                            Ok(event) => tracing::info!("acme: {event:?}"),
                            Err(error) => tracing::error!("acme: {error:?}"),
                        }
                    }
                });
                Self {
                    acceptor: Acceptor::Acme(acceptor),
                    server_config,
                    acme_task: Some(acme_task),
                }
            }
        };
        Ok(Some(tls))
    }

    /// `None` for a completed TLS-ALPN-01 validation connection.
    pub async fn accept(&self, stream: TcpStream) -> io::Result<Option<TlsStream<TcpStream>>> {
        match &self.acceptor {
            Acceptor::Manual(acceptor) => acceptor.accept(stream).await.map(Some),
            Acceptor::Acme(acceptor) => match acceptor.accept(stream).await? {
                None => Ok(None),
                Some(handshake) => handshake
                    .into_stream(self.server_config.clone())
                    .await
                    .map(Some),
            },
        }
    }
}
