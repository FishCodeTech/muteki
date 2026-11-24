use std::sync::atomic::Ordering;

use super::super::tests::{tree, write_older_data_dir};
use super::*;
use crate::fs_tree::CHUNK_BYTES;

/// An older data dir and the data dir to move it into, under one temporary
/// directory removed when the test ends.
struct Dirs {
    base: PathBuf,
    previous: PathBuf,
    root: PathBuf,
}

impl Dirs {
    fn new() -> Self {
        let base =
            std::env::temp_dir().join(format!("tcode-relocate-test-{}", uuid::Uuid::new_v4()));
        let previous = base.join("old");
        let root = base.join("new");
        write_older_data_dir(&previous);
        fs::create_dir_all(root.join("worktrees")).unwrap();
        Self {
            base,
            previous,
            root,
        }
    }

    fn run(
        &self,
        report: &mut dyn FnMut(MigrationProgress),
        cancel: &AtomicBool,
    ) -> io::Result<Migration> {
        run(&self.root, Some(&self.previous), report, cancel)
    }

    fn run_to_completion(&self) {
        assert_eq!(
            self.run(&mut |_| {}, &AtomicBool::new(false)).unwrap(),
            Migration::Completed
        );
    }

    /// What a finished move leaves in the data dir: the older entries
    /// without what is not data, and the data dir's own.
    fn moved(
        &self,
        older: &std::collections::BTreeMap<PathBuf, Vec<u8>>,
    ) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut expected = older.clone();
        expected.retain(|path, _| !LEFT_BEHIND.iter().any(|name| path == Path::new(name)));
        expected.insert("worktrees".into(), b"<dir>".to_vec());
        expected
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = remove_if_present(&self.base);
    }
}

#[test]
fn a_name_in_both_directories_stops_the_move_before_anything_moves() {
    let dirs = Dirs::new();
    fs::create_dir(dirs.root.join("attachments")).unwrap();
    let (previous, root) = (tree(&dirs.previous), tree(&dirs.root));

    let error = dirs.run(&mut |_| {}, &AtomicBool::new(false)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(error.to_string().contains("attachments"), "{error}");

    let mut after = tree(&dirs.previous);
    // Taken to keep an older build out while moving.
    assert!(after.remove(Path::new(LOCK_FILE)).is_some());
    assert_eq!(after, previous);
    assert_eq!(tree(&dirs.root), root);
    assert!(needed(&dirs.root, Some(&dirs.previous)).unwrap());
}

#[test]
fn a_cancelled_move_keeps_what_it_moved_and_the_next_start_finishes_it() {
    let dirs = Dirs::new();
    let older = tree(&dirs.previous);
    let cancel = AtomicBool::new(false);
    let outcome = dirs
        .run(
            &mut |progress| {
                if progress.threads_done == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .unwrap();
    assert_eq!(outcome, Migration::Cancelled);

    // Nothing lost: every older entry is in exactly one of the two.
    let (left, moved) = (tree(&dirs.previous), tree(&dirs.root));
    for (path, bytes) in dirs.moved(&older) {
        let found = [left.get(&path), moved.get(&path)];
        assert!(
            found.iter().flatten().count() == 1
                && found.iter().flatten().all(|found| **found == bytes),
            "{path:?}: {found:?}"
        );
    }
    assert!(moved.contains_key(Path::new("attachments")));
    assert!(left.contains_key(Path::new("settings.json")));
    assert!(needed(&dirs.root, Some(&dirs.previous)).unwrap());

    dirs.run_to_completion();
    assert!(!dirs.previous.exists());
    assert_eq!(tree(&dirs.root), dirs.moved(&older));
    assert!(!needed(&dirs.root, Some(&dirs.previous)).unwrap());
}

/// The branch a failed `rename` across filesystems takes, on one entry with
/// nested directories, a file of several chunks, an empty file and a link.
#[test]
fn a_copied_entry_arrives_verified_and_its_source_is_deleted() {
    let dirs = Dirs::new();
    let attachments = dirs.previous.join("attachments");
    let large: Vec<u8> = (0..CHUNK_BYTES + 4096).map(|index| index as u8).collect();
    fs::write(attachments.join("session-1/large.bin"), &large).unwrap();
    fs::write(attachments.join("empty"), b"").unwrap();
    // A Go module cache in a profile home is read-only, directories included.
    let module = attachments.join("mod/pkg@v1");
    fs::create_dir_all(&module).unwrap();
    fs::write(module.join("go.mod"), b"module pkg").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::os::unix::fs::symlink("session-1/a.png", attachments.join("latest")).unwrap();
        fs::set_permissions(module.join("go.mod"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(&module, fs::Permissions::from_mode(0o555)).unwrap();
    }
    let source = tree(&attachments);
    fs::create_dir(dirs.root.join(STAGING_DIR)).unwrap();

    let mut reports = Vec::new();
    let mut report = |progress| reports.push(progress);
    let cancel = AtomicBool::new(false);
    let mut progress = Progress {
        report: &mut report,
        cancel: &cancel,
        current: MigrationProgress {
            phase: MigrationPhase::Relocating,
            threads_done: 0,
            threads_total: 1,
            bytes_done: 0,
            bytes_total: tree_size(&attachments).unwrap(),
        },
    };
    copy_entry(
        &dirs.root,
        &dirs.previous,
        "attachments".as_ref(),
        &mut progress,
    )
    .unwrap();

    assert_eq!(tree(&dirs.root.join("attachments")), source);
    assert!(!attachments.exists());
    assert!(
        fs::read_dir(dirs.root.join(STAGING_DIR))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(!dirs.previous.join(RETIRED_DIR).join("attachments").exists());
    let last = reports.last().unwrap();
    assert_eq!(last.bytes_done, last.bytes_total);
    assert_eq!(last.bytes_total, large.len() as u64 + 4 + 10);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let copied = dirs.root.join("attachments/mod/pkg@v1");
        assert_eq!(
            (mode(&copied), mode(&copied.join("go.mod"))),
            (0o555, 0o444)
        );
    }
}

/// Killed while copying: one entry's copy is staged but partial, another's
/// is complete and its source already retired.
#[test]
fn a_start_after_an_interrupted_copy_recopies_a_partial_file_and_publishes_a_verified_one() {
    let dirs = Dirs::new();
    let older = tree(&dirs.previous);
    let staging = dirs.root.join(STAGING_DIR);
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("settings.json"), br#"{"loc"#).unwrap();
    fs::create_dir(dirs.previous.join(RETIRED_DIR)).unwrap();
    fs::rename(
        dirs.previous.join("profile-homes"),
        dirs.previous.join(RETIRED_DIR).join("profile-homes"),
    )
    .unwrap();
    fs::create_dir_all(staging.join("profile-homes/claude/.claude")).unwrap();
    fs::write(
        staging.join("profile-homes/claude/.claude/settings.json"),
        b"{}",
    )
    .unwrap();
    // And one entry already published before the kill.
    fs::rename(
        dirs.previous.join("project-icons"),
        dirs.root.join("project-icons"),
    )
    .unwrap();

    assert!(needed(&dirs.root, Some(&dirs.previous)).unwrap());
    dirs.run_to_completion();
    assert!(!dirs.previous.exists());
    assert_eq!(tree(&dirs.root), dirs.moved(&older));
}

#[test]
fn the_same_directory_under_another_name_is_not_moved() {
    let dirs = Dirs::new();
    let before = tree(&dirs.previous);
    for alias in [dirs.previous.join("."), dirs.base.join("old/../old")] {
        assert_eq!(
            run(
                &dirs.previous,
                Some(&alias),
                &mut |_| {},
                &AtomicBool::new(false)
            )
            .unwrap(),
            Migration::Completed
        );
    }
    #[cfg(unix)]
    {
        let link = dirs.base.join("link");
        std::os::unix::fs::symlink(&dirs.previous, &link).unwrap();
        assert!(!needed(&link, Some(&dirs.previous)).unwrap());
    }
    assert_eq!(tree(&dirs.previous), before);
}
