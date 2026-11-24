//! Machine groups over real loopback endpoints: two machines with Traverse
//! off, a desktop that hosts beside its own window, and phones that pair
//! with one member and reach the others.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tcode_client::ConnectionFailure;
use tcode_client::ConnectionState;
use tcode_client::host::Transport;
use tcode_client::pairing::{PairInvite, PairedHost, parse_pair_url};
use tcode_protocol::{GroupAction, HostingAction, HostingState};
use tcode_traverse::{DeviceIdentity, HostConfig, HostMux, TraverseHost};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tcode-group-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A machine whose application side is never asked anything: these tests
/// only open connections.
struct Machine {
    host: TraverseHost,
    _channels: (
        async_channel::Receiver<tcode_client::attachment::AttachmentMessage>,
        async_channel::Sender<String>,
    ),
}

fn machine(dir: &TestDir, name: &str) -> Machine {
    let (to_host, host_rx) =
        async_channel::unbounded::<tcode_client::attachment::AttachmentMessage>();
    let (host_tx, from_host) = async_channel::unbounded::<String>();
    let host = TraverseHost::start(
        HostMux::new(to_host, from_host),
        HostConfig {
            host_name: name.into(),
            data_dir: dir.0.clone(),
            traverse: Vec::new(),
            pairing_enabled: true,
            bind_port: None,
        },
    )
    .unwrap();
    Machine {
        host,
        _channels: (host_rx, host_tx),
    }
}

/// A device that finds machines only at the addresses it saved, as a phone
/// off any service does once nothing else answers.
fn device(dir: &TestDir, name: &str) -> DeviceIdentity {
    let device = DeviceIdentity::load_or_create(&dir.0).unwrap();
    device.set_details(name.into(), None);
    device.set_lan_options(tcode_traverse::lan::LanOptions {
        browse: tcode_traverse::lan::Browse::Off,
        multicast_lock: None,
    });
    device
}

fn group(machine: &Machine, action: GroupAction) -> HostingState {
    machine
        .host
        .hosting(HostingAction::Group(action))
        .unwrap_or_else(|error| panic!("{error:?}"))
}

fn state(machine: &Machine) -> HostingState {
    machine.host.hosting(HostingAction::State).unwrap()
}

/// A fresh one-time invitation, as Settings → Remote shows it.
fn invitation(machine: &Machine) -> String {
    machine.host.new_invitation().url()
}

/// Pair with an invitation at the address typed for it, and save the
/// machine as the pairing screen does.
fn pair(link: &str, device: &DeviceIdentity) -> PairedHost {
    let invite: PairInvite = parse_pair_url(link).unwrap();
    let paired =
        tcode_traverse::pair_blocking(&invite, Some(std::net::Ipv4Addr::LOCALHOST.into()), device)
            .unwrap();
    tcode_traverse::hosts::update_hosts(device.data_dir(), |hosts| {
        tcode_client::pairing::remember_host(hosts, paired.clone());
    })
    .unwrap();
    paired
}

fn saved(device: &DeviceIdentity, machine: &Machine) -> PairedHost {
    let id = machine.host.endpoint_id();
    tcode_traverse::hosts::load_hosts(device.data_dir())
        .unwrap()
        .into_iter()
        .find(|host| host.host_id == id)
        .unwrap_or_else(|| panic!("{id} is not saved"))
}

fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("never saw {what}");
}

fn admits(machine: &Machine, device: &DeviceIdentity) -> bool {
    let id = device.endpoint_id().to_string();
    machine.host.devices().iter().any(|record| record.id == id)
}

fn wait_state(transport: &Transport, wanted: impl Fn(&ConnectionState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Ok(state) = transport.state.try_recv() {
            if wanted(&state) {
                return;
            }
            seen.push(state);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("did not observe the wanted state; saw {seen:?}");
}

fn up(state: &ConnectionState) -> bool {
    matches!(
        state,
        ConnectionState::Syncing { .. } | ConnectionState::Connected { .. }
    )
}

fn rejected(state: &ConnectionState) -> bool {
    state
        == &ConnectionState::Offline {
            reason: ConnectionFailure::AuthenticationRejected,
        }
}

/// #935's scenario: the headless machine starts a group with the device
/// already paired to it, the desktop joins with the headless machine's
/// invitation and brings its window, a phone pairs with the desktop's
/// invitation and reaches the headless machine too; once the headless
/// machine has left, the phone still reaches the desktop, and a new phone
/// pairs with the desktop alone.
#[test]
fn a_device_paired_with_one_member_reaches_every_member_and_outlives_the_first() {
    let headless_dir = TestDir::new("headless");
    let headless = machine(&headless_dir, "Docker");
    let desktop_dir = TestDir::new("desktop");
    let window = device(&desktop_dir, "Desktop window");
    let desktop = machine(&desktop_dir, "Desktop");
    let early_dir = TestDir::new("early");
    let early = device(&early_dir, "Early phone");
    pair(&invitation(&headless), &early);

    let created = group(
        &headless,
        GroupAction::Create {
            name: "Home".into(),
        },
    );
    let home = created.group.unwrap();
    assert_eq!(home.machines.len(), 1);
    assert!(home.machines[0].this_machine);
    let link = invitation(&headless);
    assert_eq!(parse_pair_url(&link).unwrap().group, Some(home.id.clone()));
    let joined = group(
        &desktop,
        GroupAction::Join {
            link,
            address: Some("127.0.0.1".into()),
        },
    );
    assert_eq!(joined.group.as_ref().unwrap().machines.len(), 2);
    assert_eq!(state(&headless).group.unwrap().machines.len(), 2);
    assert!(
        admits(&headless, &window),
        "the desktop brings its own window"
    );
    assert!(
        !admits(&desktop, &window),
        "the window reaches its own machine without the network"
    );
    assert!(
        admits(&desktop, &early),
        "a device paired before the group was created is a member"
    );
    let from_window = saved(&window, &headless);
    assert_eq!(from_window.group_id.as_deref(), Some(home.id.as_str()));
    let client = tcode_traverse::connect(&from_window, &window);
    wait_state(&client, up);
    client.to_host.close();

    let phone_dir = TestDir::new("phone");
    let phone = device(&phone_dir, "Phone");
    let paired = pair(&invitation(&desktop), &phone);
    assert_eq!(paired.name, "Desktop");
    assert_eq!(paired.group_id.as_deref(), Some(home.id.as_str()));
    assert!(admits(&desktop, &phone));
    eventually("the headless machine admit the phone", || {
        admits(&headless, &phone)
    });
    let to_headless = tcode_traverse::connect(&saved(&phone, &headless), &phone);
    wait_state(&to_headless, up);
    to_headless.to_host.close();

    group(&headless, GroupAction::Leave);
    assert!(state(&headless).group.is_none());
    assert!(
        admits(&headless, &phone),
        "a machine that leaves keeps its devices as directly paired"
    );
    eventually("the desktop see the headless machine leave", || {
        state(&desktop).group.unwrap().machines.len() == 1
    });
    headless.host.shutdown();
    let to_desktop = tcode_traverse::connect(&saved(&phone, &desktop), &phone);
    wait_state(&to_desktop, up);
    to_desktop.to_host.close();

    let second_dir = TestDir::new("second-phone");
    let second = device(&second_dir, "Second phone");
    pair(&invitation(&desktop), &second);
    assert!(admits(&desktop, &second));
    desktop.host.shutdown();
}

/// Removing a device on any member removes it from every member and closes
/// its connections; only a new pairing brings it back. A member that is not
/// accepting new devices declines what the others admit until it is told to
/// admit it.
#[test]
fn removal_is_group_wide_and_accept_new_devices_governs_introductions() {
    let first_dir = TestDir::new("first");
    let first = machine(&first_dir, "First");
    let second_dir = TestDir::new("second");
    let second = machine(&second_dir, "Second");
    let home = group(
        &first,
        GroupAction::Create {
            name: "Home".into(),
        },
    )
    .group
    .unwrap();
    group(
        &second,
        GroupAction::Join {
            link: invitation(&first),
            address: Some("127.0.0.1".into()),
        },
    );
    assert_eq!(state(&second).group.unwrap().id, home.id);

    let phone_dir = TestDir::new("removed-phone");
    let phone = device(&phone_dir, "Phone");
    pair(&invitation(&second), &phone);
    eventually("the first machine admit the phone", || {
        admits(&first, &phone)
    });
    let client = tcode_traverse::connect(&saved(&phone, &first), &phone);
    wait_state(&client, up);

    let id = phone.endpoint_id().to_string();
    let revoked = second
        .host
        .hosting(HostingAction::RevokeDevice(id.clone()))
        .unwrap();
    assert!(!revoked.devices.iter().any(|device| device.id == id));
    wait_state(&client, rejected);
    assert!(!admits(&first, &phone));
    client.to_host.close();

    pair(&invitation(&first), &phone);
    eventually("the second machine admit the phone again", || {
        admits(&second, &phone)
    });

    first.host.set_pairing_enabled(false);
    let late_dir = TestDir::new("late-phone");
    let late = device(&late_dir, "Late phone");
    pair(&invitation(&second), &late);
    let late_id = late.endpoint_id().to_string();
    eventually("the first machine decline the late phone", || {
        state(&first)
            .group
            .unwrap()
            .declined
            .iter()
            .any(|device| device.id == late_id)
    });
    assert!(!admits(&first, &late));
    first.host.set_pairing_enabled(true);
    // Applying the group with the switch back on, as pairing another device
    // does, does not admit what was declined.
    let other_dir = TestDir::new("other-phone");
    pair(&invitation(&first), &device(&other_dir, "Other phone"));
    assert!(!admits(&first, &late));
    let admitted = group(&first, GroupAction::Admit(late_id.clone()));
    assert!(admitted.group.unwrap().declined.is_empty());
    assert!(admits(&first, &late));

    // A machine another member removes leaves as if it had left itself, and
    // keeps its devices as directly paired.
    group(
        &first,
        GroupAction::RemoveMachine(second.host.endpoint_id()),
    );
    assert_eq!(state(&first).group.unwrap().machines.len(), 1);
    eventually("the removed machine leave the group", || {
        state(&second).group.is_none()
    });
    assert!(admits(&second, &late));

    first.host.shutdown();
    second.host.shutdown();
}
