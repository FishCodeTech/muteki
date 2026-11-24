use super::*;

fn fixture() -> (tempfile::TempDir, Transfers, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path().join("node.sqlite")).unwrap();
    let target = root.path().join("workspace");
    std::fs::create_dir(&target).unwrap();
    let transfers = Transfers::open(store, root.path(), 64).unwrap();
    (root, transfers, target)
}
fn manifest(id: &str, path: &str, bytes: &[u8]) -> Manifest {
    Manifest {
        id: id.into(),
        scope: Scope {
            node_id: "node".into(),
            project_id: "project".into(),
            run_id: "run".into(),
            generation: 1,
        },
        path: path.into(),
        size: bytes.len() as u64,
        sha256: digest(bytes),
        mode: 0o644,
    }
}
#[test]
fn resumes_after_reopen_and_does_not_publish_partial_or_changed_chunks() {
    let (root, transfers, target) = fixture();
    let mut input = manifest("file", "bin/tool", b"abcdef");
    input.mode = 0o755;
    transfers.begin(&input).unwrap();
    transfers
        .chunk(&input.id, &input.scope, 0, b"abc", &digest(b"abc"))
        .unwrap();
    assert!(transfers.commit(&input.id, &input.scope, &target).is_err());
    assert!(!target.join("bin/tool").exists());
    OpenOptions::new()
        .append(true)
        .open(transfers.path(&input.id, "part"))
        .unwrap()
        .write_all(b"uncommitted")
        .unwrap();
    drop(transfers);
    let reopened = Transfers::open(
        Store::open(root.path().join("node.sqlite")).unwrap(),
        root.path(),
        64,
    )
    .unwrap();
    assert_eq!(reopened.begin(&input).unwrap()["received"], 3);
    assert_eq!(
        reopened
            .chunk(&input.id, &input.scope, 0, b"abc", &digest(b"abc"))
            .unwrap()["received"],
        3
    );
    assert!(
        reopened
            .chunk(&input.id, &input.scope, 0, b"xyz", &digest(b"xyz"))
            .is_err()
    );
    assert!(
        reopened
            .chunk(&input.id, &input.scope, 3, b"def", &digest(b"wrong"))
            .is_err()
    );
    reopened
        .chunk(&input.id, &input.scope, 3, b"def", &digest(b"def"))
        .unwrap();
    let result = reopened.commit(&input.id, &input.scope, &target).unwrap();
    assert_eq!(result["state"], "committed");
    assert_eq!(std::fs::read(target.join("bin/tool")).unwrap(), b"abcdef");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(target.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
    assert_eq!(
        reopened.commit(&input.id, &input.scope, &target).unwrap()["state"],
        "committed"
    );
    assert!(reopened.cancel(&input.id, &input.scope).is_err());
}
#[test]
fn scope_quota_cancel_and_manifest_identity_are_enforced() {
    let (_root, transfers, _target) = fixture();
    let input = manifest("file", "input", &[0; 40]);
    transfers.begin(&input).unwrap();
    let mut wrong = input.scope.clone();
    wrong.run_id = "another".into();
    assert!(transfers.status(&input.id, &wrong).is_err());
    let mut changed = input.clone();
    changed.path = "elsewhere".into();
    assert!(transfers.begin(&changed).is_err());
    let second = manifest("second", "second", &[1; 40]);
    assert!(transfers.begin(&second).is_err());
    assert!(!transfers.path("second", "lock").exists());
    assert!(transfers.status("unknown", &input.scope).is_err());
    assert!(!transfers.path("unknown", "lock").exists());
    transfers.cancel(&input.id, &input.scope).unwrap();
    assert!(!transfers.path(&input.id, "part").exists());
    transfers.begin(&second).unwrap();
    assert!(
        transfers
            .chunk(&input.id, &input.scope, 0, b"x", &digest(b"x"))
            .is_err()
    );
}
#[test]
fn failed_commit_preserves_existing_files_and_recovers_a_lost_commit_receipt() {
    let (_root, transfers, target) = fixture();
    let input = manifest("file", "artifact", b"new");
    transfers.begin(&input).unwrap();
    transfers
        .chunk(&input.id, &input.scope, 0, b"new", &digest(b"new"))
        .unwrap();
    std::fs::write(target.join("artifact"), b"old").unwrap();
    assert!(transfers.commit(&input.id, &input.scope, &target).is_err());
    assert_eq!(std::fs::read(target.join("artifact")).unwrap(), b"old");
    std::fs::remove_file(target.join("artifact")).unwrap();
    secure::install(&target, &input, &mut transfers.staging(&input.id).unwrap()).unwrap();
    assert_eq!(
        transfers.commit(&input.id, &input.scope, &target).unwrap()["state"],
        "committed"
    );
}
#[test]
fn validates_complete_hash_and_recovers_reserved_zero_offset_file() {
    let (_root, transfers, target) = fixture();
    let input = manifest("file", "artifact", b"abc");
    transfers.begin(&input).unwrap();
    std::fs::remove_file(transfers.path(&input.id, "part")).unwrap();
    transfers.begin(&input).unwrap();
    transfers
        .chunk(&input.id, &input.scope, 0, b"xyz", &digest(b"xyz"))
        .unwrap();
    assert!(transfers.commit(&input.id, &input.scope, &target).is_err());
    assert!(!target.join("artifact").exists());
}
#[test]
fn rejects_escape_and_private_paths_before_reserving_disk() {
    let (_root, transfers, _target) = fixture();
    for path in [
        "/absolute",
        "../escape",
        "a/../escape",
        "a//b",
        "a\\b",
        "C:/windows",
        "./file",
        "a/\0b",
        "_secrets/key",
        ".muteki-browser-profile/Cookies",
    ] {
        assert!(
            transfers.begin(&manifest("bad", path, b"x")).is_err(),
            "{path:?}"
        );
    }
    assert!(transfers.store.list(TABLE).unwrap().is_empty());
}
#[test]
fn quota_reservation_is_atomic_between_independent_store_connections() {
    let (root, transfers, _target) = fixture();
    let second = Transfers::open(
        Store::open(root.path().join("node.sqlite")).unwrap(),
        root.path(),
        64,
    )
    .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let other = barrier.clone();
    let thread = std::thread::spawn(move || {
        other.wait();
        second.begin(&manifest("second", "second", &[2; 40]))
    });
    barrier.wait();
    let first = transfers.begin(&manifest("first", "first", &[1; 40]));
    assert_eq!(
        usize::from(first.is_ok()) + usize::from(thread.join().unwrap().is_ok()),
        1
    );
}
#[test]
#[cfg(unix)]
fn directory_and_final_symlinks_cannot_escape_workspace() {
    let (root, transfers, target) = fixture();
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, target.join("linked")).unwrap();
    let input = manifest("link", "linked/escape", b"x");
    transfers.begin(&input).unwrap();
    transfers
        .chunk(&input.id, &input.scope, 0, b"x", &digest(b"x"))
        .unwrap();
    assert!(transfers.commit(&input.id, &input.scope, &target).is_err());
    assert!(!outside.join("escape").exists());
    std::fs::write(outside.join("secret"), b"unchanged").unwrap();
    std::os::unix::fs::symlink(outside.join("secret"), target.join("final")).unwrap();
    let input = manifest("final", "final", b"x");
    transfers.begin(&input).unwrap();
    transfers
        .chunk(&input.id, &input.scope, 0, b"x", &digest(b"x"))
        .unwrap();
    assert!(transfers.commit(&input.id, &input.scope, &target).is_err());
    assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"unchanged");
}
#[test]
fn publication_guard_can_revoke_after_copy_without_publishing_partial_data() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Held(Arc<AtomicUsize>);
    impl Drop for Held {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let (_root, transfers, target) = fixture();
    let input = manifest("guarded", "artifact", b"complete");
    transfers.begin(&input).unwrap();
    transfers
        .chunk(
            &input.id,
            &input.scope,
            0,
            b"complete",
            &digest(b"complete"),
        )
        .unwrap();
    let calls = AtomicUsize::new(0);
    let held = Arc::new(AtomicUsize::new(0));
    let result = transfers.commit_guarded(&input.id, &input.scope, &target, || {
        assert_eq!(
            held.load(Ordering::SeqCst),
            0,
            "Lifecycle guard leaked across copy phases"
        );
        if calls.fetch_add(1, Ordering::SeqCst) == 2 {
            return Err("Execution stopped before publication".into());
        }
        held.fetch_add(1, Ordering::SeqCst);
        Ok(Held(held.clone()))
    });
    assert!(result.unwrap_err().contains("stopped"));
    assert_eq!(held.load(Ordering::SeqCst), 0);
    assert!(!target.join("artifact").exists());
    assert!(!target.join(".muteki-transfer-guarded").exists());
    assert_eq!(
        transfers.status(&input.id, &input.scope).unwrap()["state"],
        "verified"
    );
    transfers.commit(&input.id, &input.scope, &target).unwrap();
}
#[test]
#[cfg(unix)]
fn interrupted_destination_temp_is_replaced_without_following_its_link() {
    let (root, transfers, target) = fixture();
    let input = manifest("retry", "artifact", b"new");
    transfers.begin(&input).unwrap();
    transfers
        .chunk(&input.id, &input.scope, 0, b"new", &digest(b"new"))
        .unwrap();
    let outside = root.path().join("unrelated");
    std::fs::write(&outside, b"preserved").unwrap();
    std::os::unix::fs::symlink(&outside, target.join(".muteki-transfer-retry")).unwrap();
    transfers.commit(&input.id, &input.scope, &target).unwrap();
    assert_eq!(std::fs::read(&outside).unwrap(), b"preserved");
    assert_eq!(std::fs::read(target.join("artifact")).unwrap(), b"new");
    assert!(!target.join(".muteki-transfer-retry").exists());
    assert!(
        transfers
            .begin(&manifest("bad-temp", ".muteki-transfer-reserved", b"x"))
            .is_err()
    );
}

#[test]
fn artifact_snapshot_is_bounded_resumable_and_immutable() {
    use std::cell::Cell;
    let (_root, transfers, workspace)=fixture();
    let source=workspace.join("result.txt");
    std::fs::write(&source,b"artifact").unwrap();
    let scope=manifest("output","result.txt",b"").scope;
    assert!(transfers.snapshot("too-large",&scope,&workspace,"result.txt",3,||Ok(())).is_err());
    assert!(transfers.manifest("too-large").is_err());
    let calls=Cell::new(0);
    assert!(transfers.snapshot("output",&scope,&workspace,"result.txt",64,|| {
        calls.set(calls.get()+1);
        if calls.get()==5 {Err("lease expired".into())} else {Ok(())}
    }).is_err());
    assert_eq!(transfers.status("output",&scope).unwrap()["state"],"receiving");
    let snapshot=transfers.snapshot("output",&scope,&workspace,"result.txt",64,||Ok(())).unwrap();
    assert_eq!(snapshot.size,8);
    assert_eq!(snapshot.mode,0o600);
    assert_eq!(snapshot.sha256,digest(b"artifact"));
    std::fs::write(&source,b"modified").unwrap();
    assert_eq!(transfers.read_chunk("output",&scope,0,8).unwrap(),b"artifact");
    assert!(transfers.snapshot("output",&scope,&workspace,"result.txt",64,||Ok(())).is_err());
}

#[test]
fn artifact_change_between_hash_and_copy_cannot_be_sealed() {
    use std::cell::Cell;
    let (_root,transfers,workspace)=fixture();
    let path=workspace.join("changing");
    std::fs::write(&path,b"before").unwrap();
    let scope=manifest("changing","changing",b"").scope;
    let calls=Cell::new(0);
    assert!(transfers.snapshot("changing",&scope,&workspace,"changing",64,|| {
        calls.set(calls.get()+1);
        if calls.get()==5 {std::fs::write(&path,b"after!").unwrap();}
        Ok(())
    }).is_err());
    assert_eq!(transfers.status("changing",&scope).unwrap()["state"],"receiving");
    assert!(transfers.read_chunk("changing",&scope,0,6).is_err());
}

#[cfg(unix)]
#[test]
fn artifact_paths_reject_links_special_files_and_private_roots() {
    use std::os::unix::fs::symlink;
    let (root,transfers,workspace)=fixture();
    let external=root.path().join("external");
    std::fs::write(&external,b"private").unwrap();
    symlink(&external,workspace.join("symlink")).unwrap();
    symlink(root.path(),workspace.join("directory")).unwrap();
    std::fs::hard_link(&external,workspace.join("hardlink")).unwrap();
    let pipe=std::ffi::CString::new(workspace.join("pipe").to_str().unwrap()).unwrap();
    assert_eq!(unsafe{libc::mkfifo(pipe.as_ptr(),0o600)},0);
    std::fs::create_dir_all(workspace.join("homes/worker/.codex")).unwrap();
    std::fs::write(workspace.join("homes/worker/.codex/auth.json"),b"credential").unwrap();
    assert!(validate_path("homes/worker/.codex/auth.json").is_err());
    assert!(validate_path("inputs/homes/example.txt").is_ok());
    let scope=manifest("output","result",b"").scope;
    for path in ["../external","symlink","directory/external","hardlink","pipe","_secrets/key","homes/worker/.codex/auth.json",".muteki-transfer-output"] {
        assert!(transfers.snapshot("output",&scope,&workspace,path,64,||Ok(())).is_err(),"{path}");
    }
    assert!(transfers.manifest("output").is_err());
}
