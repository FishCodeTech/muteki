//! `hosts.json`: the machines this device has paired with. Field-level
//! updates from the UI and the transport, possibly from several app processes
//! sharing one profile, serialize on a lock file; readers see either complete
//! version via rename.
use std::{fs, io, path::Path};

use tcode_client::pairing::{MAX_ADDRS, PairedHost, valid_host_id};

/// The saved machines. A record whose id is not a Traverse machine id is
/// left out: the previous transport's `hosts.json` named machines by UUID,
/// and dialling one of those can never succeed.
pub fn load_hosts(data_dir: &Path) -> io::Result<Vec<PairedHost>> {
    let hosts: Vec<PairedHost> = match fs::read(data_dir.join("hosts.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(hosts
        .into_iter()
        .filter(|host| {
            let valid = valid_host_id(&host.host_id);
            if !valid {
                warn_once(&host.host_id, &host.name);
            }
            valid
        })
        .collect())
}

/// The list is read on every render, so an ignored record is reported once.
fn warn_once(host_id: &str, name: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static REPORTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut reported = REPORTED.get_or_init(Default::default).lock().unwrap();
    if reported.insert(host_id.to_owned()) {
        log::warn!("ignoring saved machine {name:?} ({host_id}): not a Traverse machine id");
    }
}

pub fn save_hosts(data_dir: &Path, hosts: &[PairedHost]) -> io::Result<()> {
    let _lock = hosts_lock(data_dir)?;
    write_hosts(data_dir, hosts)
}

pub fn update_hosts(data_dir: &Path, update: impl FnOnce(&mut Vec<PairedHost>)) -> io::Result<()> {
    let _lock = hosts_lock(data_dir)?;
    let mut hosts = load_hosts(data_dir)?;
    let before = hosts.clone();
    update(&mut hosts);
    if hosts != before {
        write_hosts(data_dir, &hosts)?;
    }
    Ok(())
}

/// Save what a machine of group `group_id` says about the group: every
/// member machine is kept beside the others with the group's id, and a
/// machine that is no longer a member loses it but stays saved, since it
/// still admits this device as one it paired. `roster` is `None` when the
/// machine that told this device has no group any more. `local` is the
/// machine sharing this data directory, which is never a remote machine.
pub fn apply_roster(
    hosts: &mut Vec<PairedHost>,
    from: &str,
    roster: Option<&crate::group::Roster>,
    local: Option<&str>,
) {
    let Some(roster) = roster else {
        if let Some(host) = hosts.iter_mut().find(|host| host.host_id == from) {
            host.group_id = None;
        }
        return;
    };
    for machine in &roster.machines {
        if Some(machine.id.as_str()) == local || !valid_host_id(&machine.id) {
            continue;
        }
        match hosts.iter_mut().find(|host| host.host_id == machine.id) {
            Some(host) if machine.member => {
                host.group_id = Some(roster.id.clone());
                host.name.clone_from(&machine.name);
                if host.traverse.is_empty() {
                    host.traverse.clone_from(&machine.traverse);
                }
                if host.relay.is_none() {
                    host.relay.clone_from(&machine.relay);
                }
                for addr in &machine.addrs {
                    if host.addrs.len() < MAX_ADDRS && !host.addrs.contains(addr) {
                        host.addrs.push(addr.clone());
                    }
                }
            }
            Some(host) => {
                if host.group_id.as_deref() == Some(roster.id.as_str()) {
                    host.group_id = None;
                }
            }
            None if machine.member => hosts.push(PairedHost {
                host_id: machine.id.clone(),
                name: machine.name.clone(),
                space_id: None,
                space_name: None,
                traverse: machine.traverse.clone(),
                relay: machine.relay.clone(),
                addrs: machine.addrs.iter().take(MAX_ADDRS).cloned().collect(),
                last_connected_unix: None,
                group_id: Some(roster.id.clone()),
            }),
            None => {}
        }
    }
}

fn hosts_lock(data_dir: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(data_dir)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(data_dir.join("hosts.lock"))?;
    lock_exclusive(&file)?;
    Ok(file)
}

/// Rust's standard library has no `File::lock` on Android, where it fails
/// with `Unsupported`; bionic has `flock(2)`, so call it there.
#[cfg(all(target_os = "android", feature = "native"))]
fn lock_exclusive(file: &fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: `file` is an open descriptor for the lock file; `flock` only
    // reads it and blocks until the exclusive lock is held.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(all(target_os = "android", feature = "native")))]
fn lock_exclusive(file: &fs::File) -> io::Result<()> {
    file.lock()
}

fn write_hosts(data_dir: &Path, hosts: &[PairedHost]) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let bytes = serde_json::to_vec_pretty(hosts).map_err(io::Error::other)?;
    crate::identity::write_private(&data_dir.join("hosts.json"), &bytes)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "tcode-traverse-hosts-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    const MACHINE_ID: &str = "09e5c592f3234ef329634a0baa6bfe06193be6b64801121d84242af1d27dbecf";

    fn machine(id: &str) -> PairedHost {
        PairedHost {
            space_id: None,
            space_name: None,
            host_id: id.into(),
            name: "Desk".into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["192.168.1.5:47420".into()],
            last_connected_unix: None,
            group_id: None,
        }
    }

    #[test]
    fn a_previous_transport_record_is_not_a_saved_machine() {
        let dir = temp_dir("legacy");
        fs::write(
            dir.join("hosts.json"),
            r#"[{"host_id":"94c9ac85-0a47-4387-86ba-524d3fd7995f","name":"Desk",
                "origin":"http://192.168.1.5:47420","candidates":[],"token":"t",
                "last_connected_unix":1}]"#,
        )
        .unwrap();
        assert!(load_hosts(&dir).unwrap().is_empty());

        update_hosts(&dir, |hosts| hosts.push(machine(MACHINE_ID))).unwrap();
        let saved = load_hosts(&dir).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].host_id, MACHINE_ID);
        let written = fs::read_to_string(dir.join("hosts.json")).unwrap();
        assert!(!written.contains("94c9ac85"), "{written}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saving_takes_the_lock_and_round_trips() {
        let dir = temp_dir("roundtrip");
        save_hosts(&dir, &[machine(MACHINE_ID)]).unwrap();
        assert!(dir.join("hosts.lock").exists());
        update_hosts(&dir, |hosts| {
            hosts[0].addrs.insert(0, "10.0.0.2:47420".into());
        })
        .unwrap();
        assert_eq!(load_hosts(&dir).unwrap()[0].addrs[0], "10.0.0.2:47420");
        fs::remove_dir_all(dir).unwrap();
    }
}
