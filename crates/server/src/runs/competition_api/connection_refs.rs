//! Source CommandAPI connection operations that consume opaque SecretStore refs.
use super::*;

pub(super) async fn import_browser_state(
    state: &Arc<AppState>,
    connection_id: &str,
    body: Value,
) -> ApiResult<Value> {
    commands::import(state,connection_id,&body).await
}

mod credential;
pub(super) use credential::{link_credential,revoke_credential};
pub(crate) use credential::{reconcile_credential,reconcile_direct_cleanup};

mod lifecycle;
pub(super) use lifecycle::reconcile;
mod archive;
pub(super) use archive::{reconcile_unregistered,unregister};
pub(crate) fn finish_unregistered(state: &AppState, command_id: &str) -> ApiResult<()> {
    lifecycle::finish(state,command_id).map(|_|())
}
mod commands;
pub(super) use commands::{browser_session, create, reconcile_browser_import,
    reconcile_browser_session,
    reconcile_create, recover_create};
