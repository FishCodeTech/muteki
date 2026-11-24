//! Host-owned GitHub HTTP, credentials and admission policy. Call blocking entries via HostCx::unblock.

pub mod api;
pub mod credentials;
mod forge;
pub mod graphql;
pub mod media;
mod merge_message;
pub(crate) use merge_message::remove_agent_credits;
pub mod pull_request_actions;
pub mod pull_request_reads;
pub mod pull_request_watch;
pub mod pull_requests;
mod quota;
mod read_cache;
pub mod repository;
pub mod stack_actions;
pub mod stack_rebase;

pub use api::{GitHubApi, GitHubError, RequestOptions, Response, RestRequest};
pub use credentials::{Credential, CredentialError, Credentials, Identity};
pub use forge::GitHub;
pub use read_cache::Fresh;

pub fn normalize_host(host: &str) -> Result<String, CredentialError> {
    let host = host.trim().to_ascii_lowercase();
    if !tcode_core::pull_request::dns_name(&host) {
        return Err(CredentialError::InvalidHost);
    }
    Ok(host)
}

fn digest(value: &str) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
