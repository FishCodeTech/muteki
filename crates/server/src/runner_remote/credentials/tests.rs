use super::*;
fn material(content: &[u8]) -> Material {
    Material {
        kind: "model".into(),
        files: BTreeMap::from([("token".into(), STANDARD.encode(content))]),
        env_files: BTreeMap::from([("OPENAI_API_KEY_FILE".into(), "token".into())]),
        env_dirs: BTreeMap::new(),
    }
}
fn fixture() -> (tempfile::TempDir, Vault, identity::Lease, Scope) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path().join("db.sqlite")).unwrap();
    let now = chrono::Utc::now().timestamp();
    let token =
        identity::issue_enrollment(&store, "node".into(), vec!["project".into()], 60, now).unwrap();
    let credential = identity::enroll(&store, &token, now).unwrap();
    let lease = identity::connect(&store, &credential, now).unwrap();
    let scope = Scope {
        node_id: lease.node_id.clone(),
        project_id: "project".into(),
        run_id: "run".into(),
        generation: 1,
    };
    store.put("runner_controller_runs", "run:1", &json!({
        "run_id": scope.run_id, "generation": scope.generation,
        "node_id": scope.node_id, "project_id": scope.project_id, "state": "active"
    })).unwrap();
    let vault = Vault::open(store, root.path()).unwrap();
    (root, vault, lease, scope)
}
#[test]
fn private_versions_and_refresh_cas_preserve_scope_and_newer_credentials() {
    let (_root, vault, lease, scope) = fixture();
    let now = chrono::Utc::now().timestamp();
    let first = material(b"synthetic-initial-secret");
    let row = vault.put("model", None, &first).unwrap();
    assert!(!row.to_string().contains("synthetic"));
    assert!(
        !row.to_string()
            .contains(&STANDARD.encode(b"synthetic-initial-secret"))
    );
    let a = vault.grant("model", scope.clone(), 60, now).unwrap();
    let b = vault.grant("model", scope, 60, now).unwrap();
    assert_eq!(
        vault.reveal(&a.id, &lease, now).unwrap().1.files,
        first.files
    );
    let next = material(b"synthetic-refreshed");
    let updated = vault
        .refresh(&a.id, &lease, &a.revision, &next, now)
        .unwrap();
    assert_ne!(updated.revision, a.revision);
    assert_eq!(
        vault
            .refresh(&a.id, &lease, &a.revision, &next, now)
            .unwrap()
            .revision,
        updated.revision
    );
    assert!(
        vault
            .refresh(&b.id, &lease, &b.revision, &material(b"loser"), now)
            .is_err()
    );
    assert_eq!(
        vault.metadata("model").unwrap()["revision"],
        updated.revision
    );
    assert_eq!(
        vault.reveal(&a.id, &lease, now).unwrap().1.files,
        next.files
    );
    assert_eq!(
        vault.reveal(&b.id, &lease, now).unwrap().1.files,
        first.files
    );
    vault.revoke(&a.id).unwrap();
    assert!(vault.reveal(&a.id, &lease, now).is_err());
    assert!(
        vault
            .refresh(&a.id, &lease, &updated.revision, &first, now)
            .is_err()
    );
    assert!(vault.reveal(&b.id, &lease, now + 61).is_err());
    let saved = serde_json::to_string(&vault.store.list(GRANTS).unwrap()).unwrap();
    assert!(!saved.contains("synthetic"));
}
#[test]
fn refresh_cannot_add_files_or_control_environment_and_private_files_are_bounded() {
    let (_root, vault, lease, scope) = fixture();
    let now = chrono::Utc::now().timestamp();
    let first = material(b"key");
    vault.put("model", None, &first).unwrap();
    let grant = vault.grant("model", scope.clone(), 60, now).unwrap();
    let mut changed = first.clone();
    changed
        .files
        .insert("extra".into(), STANDARD.encode(b"unexpected"));
    assert!(
        vault
            .refresh(&grant.id, &lease, &grant.revision, &changed, now)
            .is_err()
    );
    changed = first.clone();
    changed
        .env_files
        .insert("MUTEKI_CONTROL_TOKEN_FILE".into(), "token".into());
    assert!(changed.validate().is_err());
    changed = first.clone();
    changed
        .files
        .insert("../escape".into(), STANDARD.encode(b"bad"));
    assert!(changed.validate().is_err());
    assert!(material(&vec![0; MAX_BYTES + 1]).validate().is_err());
    changed = first.clone();
    changed.kind = "node".into();
    assert!(changed.validate().is_err());
    let mut foreign = scope;
    foreign.project_id = "other".into();
    assert!(vault.grant("model", foreign, 60, now).is_err());
    identity::rotate(&vault.store, &lease.node_id, now).unwrap();
    assert!(vault.reveal(&grant.id, &lease, now).is_err());
}

#[test]
fn competing_nodes_cannot_overwrite_each_others_refresh() {
    let (root, vault, lease, scope) = fixture();
    let now = chrono::Utc::now().timestamp();
    vault.put("model", None, &material(b"initial")).unwrap();
    let a = vault.grant("model", scope.clone(), 60, now).unwrap();
    let b = vault.grant("model", scope, 60, now).unwrap();
    let another = Vault::open(
        Store::open(root.path().join("db.sqlite")).unwrap(),
        root.path(),
    )
    .unwrap();
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let first_gate = gate.clone();
    let first_lease = lease.clone();
    let thread = std::thread::spawn(move || {
        first_gate.wait();
        another
            .refresh(&a.id, &first_lease, &a.revision, &material(b"first"), now)
            .is_ok()
    });
    gate.wait();
    let second = vault
        .refresh(&b.id, &lease, &b.revision, &material(b"second"), now)
        .is_ok();
    assert_eq!(usize::from(thread.join().unwrap()) + usize::from(second), 1);
}

#[test]
fn administrator_rotation_fences_old_grants_and_revoke_removes_private_versions() {
    let (root, vault, lease, scope) = fixture();
    let now = chrono::Utc::now().timestamp();
    let first = vault.put("material", None, &material(b"initial")).unwrap();
    let grant = vault.grant("material", scope.clone(), 60, now).unwrap();
    let next = vault
        .put(
            "material",
            first["revision"].as_str(),
            &material(b"rotated"),
        )
        .unwrap();
    assert!(vault.reveal(&grant.id, &lease, now).is_err());
    let new_grant = vault.grant("material", scope, 60, now).unwrap();
    assert_eq!(new_grant.revision, next["revision"].as_str().unwrap());
    assert!(
        root.path()
            .join("runner/credential-vault/material")
            .exists()
    );
    assert_eq!(
        vault.revoke_material("material").unwrap()["private_data_removed"],
        true
    );
    assert!(
        !root
            .path()
            .join("runner/credential-vault/material")
            .exists()
    );
    assert!(vault.reveal(&new_grant.id, &lease, now).is_err());
    assert!(
        vault
            .put(
                "material",
                next["revision"].as_str(),
                &material(b"resurrected")
            )
            .is_err()
    );
    assert!(
        !root
            .path()
            .join("runner/credential-vault/material")
            .exists()
    );
    assert!(
        vault
            .store
            .list("runner_credential_versions")
            .unwrap()
            .is_empty()
    );
}
