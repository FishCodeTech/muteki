use super::*;
use tcode_protocol::{CommandResponse, ProtocolError};

impl AppState {
    pub fn refresh_host_credentials(&mut self, cx: &HostCx) {
        self.host_generation += 1;
        let generation = self.host_generation;
        let forge = self.forge.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let status = host_cx.unblock(move || forge.credential_status()).await;
            host_cx.enqueue(move |state, _| {
                if generation == state.host_generation {
                    state.settings.source_control.status = status;
                }
            });
        });
    }

    /// Threads that connect from now on read the configured hosts in their tool descriptions.
    pub(super) fn name_pull_request_hosts(&self) {
        if let Some(names) = &self.mcp.pull_request_hosts {
            names.set(tcode_core::pull_request::host_names(
                &self.pull_request_hosts(),
            ));
        }
    }

    pub fn set_host_token(
        &mut self,
        host: String,
        token: Option<String>,
        email: Option<String>,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        let host = host.trim().to_ascii_lowercase();
        let kind = self.settings.source_control.kind(&host);
        let (completion, receiver) = smol::channel::bounded(1);
        self.enqueue_store_write(
            StoreWrite::SetHostToken {
                kind,
                host: host.clone(),
                token,
                email,
                completion,
            },
            cx,
        );
        let host_cx = cx.clone();
        cx.spawn_background(async move {
            receiver
                .recv()
                .await
                .map_err(|_| ProtocolError {
                    code: "store_closed".into(),
                    message: "The settings writer stopped.".into(),
                })?
                .map_err(|error| ProtocolError {
                    code: "settings_write_failed".into(),
                    message: error,
                })?;
            host_cx.enqueue(move |state, cx| {
                state.forge.forget_credential(&host);
                state.refresh_host_credentials(cx);
            });
            Ok(CommandResponse::Unit)
        })
    }
}
