use crate::{forge::Forge, settings::SettingsStore};
use std::{collections::BTreeMap, sync::Arc};
use tcode_core::{
    pull_request::{HostKind, PullRequestKey},
    settings::{HostProblem, HostSettings},
};
use tcode_protocol::PullRequestRead;
#[path = "../../tests/support/http.rs"]
mod http;
use http::Fixture;

pub(crate) fn verification_survives_restart(
    kind: HostKind,
    authority: &str,
    repository: &str,
    make: impl Fn(SettingsStore, ureq::Agent) -> Arc<dyn Forge>,
) {
    let root = std::env::temp_dir().join(format!("tcode-verification-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture::new();
    let store = SettingsStore::new(root.clone());
    let host = || {
        let host = make(SettingsStore::new(root.clone()), fixture.builder().build());
        host.configure(BTreeMap::from([(
            authority.to_owned(),
            HostSettings::new(kind),
        )]));
        host
    };
    let key = PullRequestKey {
        host: authority.into(),
        repository: repository.into(),
        number: 1,
    };
    let read = |host: &dyn Forge, path: &str| {
        host.read(
            &key,
            PullRequestRead::FileText {
                revision: "abcdef".into(),
                path: path.into(),
            },
        )
    };
    store
        .set_token(kind, authority, Some("saved-first"))
        .unwrap();
    let first = host();
    assert_eq!(
        first.credential_status()[authority].problem,
        Some(HostProblem::Unverified)
    );
    fixture
        .call(|| read(first.as_ref(), "first.txt"), 200, "", b"file")
        .0
        .unwrap();
    assert_eq!(first.credential_status()[authority].problem, None);
    drop(first);
    let restarted = host();
    assert_eq!(
        restarted.credential_status()[authority].problem,
        None,
        "verification survives a fresh host"
    );
    fixture
        .call(|| read(restarted.as_ref(), "rejected.txt"), 401, "", b"{}")
        .0
        .unwrap_err();
    assert_eq!(
        host().credential_status()[authority].problem,
        Some(HostProblem::Unverified),
        "rejection clears persisted verification"
    );
    store
        .set_token(kind, authority, Some("saved-second"))
        .unwrap();
    assert_eq!(
        host().credential_status()[authority].problem,
        Some(HostProblem::Unverified)
    );
    // A response for the old credential must not verify the replacement saved while it ran.
    let current = host();
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| read(current.as_ref(), "pending.txt"));
        let exchange = fixture.next();
        store
            .set_token(kind, authority, Some("saved-third"))
            .unwrap();
        exchange.reply(200, "", b"file");
        pending.join().unwrap().unwrap();
    });
    assert_eq!(
        host().credential_status()[authority].problem,
        Some(HostProblem::Unverified)
    );
    let third = host();
    fixture
        .call(|| read(third.as_ref(), "third.txt"), 200, "", b"file")
        .0
        .unwrap();
    assert_eq!(host().credential_status()[authority].problem, None);
    store.set_token(kind, authority, None).unwrap();
    store
        .set_token(kind, authority, Some("saved-third"))
        .unwrap();
    assert_eq!(
        host().credential_status()[authority].problem,
        Some(HostProblem::Unverified),
        "clearing and restoring a token does not resurrect verification"
    );
    std::fs::remove_dir_all(root).unwrap();
}
