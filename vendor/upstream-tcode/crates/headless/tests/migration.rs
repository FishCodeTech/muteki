//! `tcode-headless serve` runs the one-time migration into `tcode.db` before
//! anything else starts, and an interrupt cancels it.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Every file in `root` with its bytes.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let bytes = if entry.file_type().unwrap().is_dir() {
                b"<dir>".to_vec()
            } else {
                fs::read(entry.path()).unwrap()
            };
            (entry.file_name().into_string().unwrap(), bytes)
        })
        .collect()
}

#[test]
fn sigint_cancels_the_startup_migration_and_leaves_every_source_as_it_was() {
    let root: PathBuf = std::env::temp_dir().join(format!(
        "tcode-headless-migration-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("sessions.json"),
        r#"{"projects":[],"sessions":[{"id":"log-0","title":"Zero","provider":"codex","cwd":"/work","created_at":1,"updated_at":1}]}"#,
    )
    .unwrap();
    // Enough that the migration is still importing well after it starts.
    let line = format!(
        "{{\"ts\":1,\"event\":{{\"type\":\"turn_started\",\"turn_id\":\"{}\"}}}}\n",
        "x".repeat(2000)
    );
    for index in 0..24 {
        fs::write(root.join(format!("log-{index}.jsonl")), line.repeat(1000)).unwrap();
    }
    let sources = snapshot(&root);

    let mut serve = tcode_services::process::command(env!("CARGO_BIN_EXE_tcode-headless"))
        .args(["serve", "--traverse", "off", "--data-dir"])
        .arg(&root)
        // Would move that directory into `root`.
        .env_remove("LEGACY_TCODE_DATA_DIR")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(serve.stdout.take().unwrap()).lines();
    let mut printed = Vec::new();
    loop {
        let line = output
            .next()
            .expect("serve exited before the migration imported anything")
            .unwrap();
        let importing = line.starts_with("importing:");
        printed.push(line);
        if importing {
            break;
        }
    }
    let interrupted = tcode_services::process::command("kill")
        .args(["-INT", &serve.id().to_string()])
        .status()
        .unwrap();
    assert!(interrupted.success());
    printed.extend(output.map(Result::unwrap));
    let status = serve.wait().unwrap();

    assert!(status.success(), "{status}: {printed:#?}");
    assert!(
        printed
            .iter()
            .any(|line| line.starts_with("Migration cancelled")),
        "{printed:#?}"
    );
    assert!(
        !printed.iter().any(|line| line.starts_with("Machine id")),
        "the host started after the cancel: {printed:#?}"
    );
    let mut after = snapshot(&root);
    // The data dir's ownership lock, which every start creates.
    assert!(after.remove("tcode.lock").is_some());
    assert_eq!(after, sources);
    fs::remove_dir_all(&root).unwrap();
}
