//! Finding a paired machine again on the LAN: the saved addresses over
//! loopback with the browse off, and an explicitly ignored real DNS-SD
//! advertise and browse on this host's interfaces.
use std::{
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

use futures_lite::StreamExt as _;
use iroh::address_lookup::AddressLookup as _;
use tcode_client::{ConnectionState, host::Transport, pairing::PairedHost};
use tcode_traverse::{
    DeviceIdentity, HostConfig, HostMux, TraverseHost,
    lan::{self, Browse, LanLookup, LanOptions},
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tcode-lan-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

/// A machine whose host answers nothing: the handshake is the transport's.
fn start_host(
    dir: &TestDir,
    bind_port: Option<u16>,
) -> (
    TraverseHost,
    async_channel::Receiver<tcode_client::attachment::AttachmentMessage>,
    async_channel::Sender<String>,
) {
    let (to_host, host_rx) =
        async_channel::unbounded::<tcode_client::attachment::AttachmentMessage>();
    let (host_tx, from_host) = async_channel::unbounded::<String>();
    let host = TraverseHost::start(
        HostMux::new(to_host, from_host),
        HostConfig {
            host_name: "LAN Host".into(),
            data_dir: dir.0.clone(),
            traverse: Vec::new(),
            pairing_enabled: true,
            bind_port,
        },
    )
    .unwrap();
    (host, host_rx, host_tx)
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The states seen until `wanted` matches, and how long that took.
fn wait_state(
    transport: &Transport,
    wanted: impl Fn(&ConnectionState) -> bool,
    budget: Duration,
) -> (Vec<ConnectionState>, Duration) {
    let started = Instant::now();
    let mut seen = Vec::new();
    while started.elapsed() < budget {
        if let Ok(state) = transport.state.try_recv() {
            if wanted(&state) {
                return (seen, started.elapsed());
            }
            seen.push(state);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("did not observe the wanted state; saw {seen:?}");
}

/// A device paired with `host` whose `hosts.json` lists `addrs` for it, as
/// a fresh launch sees it: the endpoint that paired is gone, the browse is
/// off, so the saved addresses are the whole lookup.
fn relaunched_device(
    host: &TraverseHost,
    dir: &TestDir,
    addrs: Vec<String>,
) -> (DeviceIdentity, PairedHost) {
    let pairing = DeviceIdentity::load_or_create(&dir.0).unwrap();
    pairing.set_details("device".into(), None);
    let minted = host.new_invitation();
    let paired = tcode_traverse::pair_blocking(
        &minted.invite,
        Some(std::net::Ipv4Addr::LOCALHOST.into()),
        &pairing,
    )
    .unwrap();
    drop(pairing);
    let device = DeviceIdentity::load_or_create(&dir.0).unwrap();
    device.set_lan_options(LanOptions {
        browse: Browse::Off,
        multicast_lock: None,
    });
    let saved = PairedHost { addrs, ..paired };
    tcode_traverse::hosts::save_hosts(&dir.0, std::slice::from_ref(&saved)).unwrap();
    (device, saved)
}

/// Every saved address is stale — the machine restarted on another port —
/// and the browse is off: the device does not connect. There is no further
/// source to fall back on, on loopback or anywhere else.
#[test]
fn stale_addresses_without_a_browse_do_not_reach_the_machine() {
    let _ = env_logger::builder().is_test(true).try_init();
    let host_dir = TestDir::new("stale-host");
    let (host, _host_rx, _host_tx) = start_host(&host_dir, Some(free_port()));
    let device_dir = TestDir::new("stale-device");
    let (device, saved) = relaunched_device(
        &host,
        &device_dir,
        vec![
            format!("127.0.0.1:{}", free_port()),
            format!("127.0.0.1:{}", free_port()),
        ],
    );

    let client = tcode_traverse::connect(&saved, &device);
    let (mut seen, took) = wait_state(
        &client,
        |state| matches!(state, ConnectionState::Reconnecting { .. }),
        Duration::from_secs(40),
    );
    eprintln!("first attempt failed after {took:?}; saw {seen:?}");
    // The retries that follow have nothing new to try either.
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if let Ok(state) = client.state.try_recv() {
            seen.push(state);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !seen.iter().any(|state| matches!(
            state,
            ConnectionState::Syncing { .. } | ConnectionState::Connected { .. }
        )),
        "the device connected without a working address: {seen:?}"
    );
    assert!(
        host.devices().iter().all(|device| device.live.is_none()),
        "the machine sees a device: {:?}",
        host.devices()
    );
    client.to_host.close();
    host.shutdown();
}

#[test]
fn a_saved_address_further_down_the_list_reaches_the_machine() {
    let _ = env_logger::builder().is_test(true).try_init();
    let host_dir = TestDir::new("saved-host");
    let port = free_port();
    let (host, _host_rx, _host_tx) = start_host(&host_dir, Some(port));
    let device_dir = TestDir::new("saved-device");
    let current = format!("127.0.0.1:{port}");
    let (device, saved) = relaunched_device(
        &host,
        &device_dir,
        vec![
            format!("127.0.0.1:{}", free_port()),
            format!("127.0.0.1:{}", free_port()),
            current.clone(),
        ],
    );

    let client = tcode_traverse::connect(&saved, &device);
    wait_state(
        &client,
        |state| matches!(state, ConnectionState::Syncing { .. }),
        Duration::from_secs(20),
    );
    let reached = host.devices();
    assert!(
        reached[0].live.as_ref().is_some_and(|path| path.direct),
        "the machine sees a direct path: {reached:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let saved = tcode_traverse::hosts::load_hosts(&device_dir.0).unwrap();
        if saved[0]
            .addrs
            .first()
            .is_some_and(|addr| addr.ends_with(&format!(":{port}")))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "working address was not saved: {:?}",
            saved[0].addrs
        );
        std::thread::yield_now();
    }
    client.to_host.close();
    host.shutdown();
}

/// A platform browse delivers what it finds while the resolve is polled, and
/// is told to stop the moment the resolve is dropped: iroh drops a resolve
/// as soon as it has a path, which aborts the task behind it, so the stop
/// must not wait for the task's deadline.
#[test]
fn a_dropped_resolve_stops_the_platform_browse_at_once() {
    let id = iroh::SecretKey::from_bytes(&[5; 32]).public();
    let requests = std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), Vec::new())));
    let browser = std::sync::Arc::new(lan::SystemBrowser::new(
        {
            let requests = requests.clone();
            move |request| requests.lock().unwrap().0.push(request)
        },
        {
            let requests = requests.clone();
            move |request| requests.lock().unwrap().1.push(request)
        },
    ));
    let lookup = LanLookup::new(
        |_| Vec::new(),
        LanOptions {
            browse: Browse::System(browser.clone()),
            multicast_lock: None,
        },
    );
    let found: SocketAddr = "10.0.0.12:47420".parse().unwrap();
    let dropped_at = tcode_traverse::block_on(async {
        let mut stream = lookup.resolve(id).unwrap();
        let request = loop {
            if let Some(request) = requests.lock().unwrap().0.first().copied() {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        browser.found(request, &id.to_string(), [found]);
        let item = stream.next().await.unwrap().unwrap();
        assert_eq!(item.provenance(), "lan-dns-sd");
        assert_eq!(
            item.endpoint_info().ip_addrs().copied().collect::<Vec<_>>(),
            [found]
        );
        assert!(requests.lock().unwrap().1.is_empty(), "the browse runs on");
        drop(stream);
        Instant::now()
    });
    let deadline = dropped_at + Duration::from_secs(1);
    while requests.lock().unwrap().1.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let (started, stopped) = requests.lock().unwrap().clone();
    assert_eq!(stopped, started, "the platform browse was stopped");
    assert!(
        dropped_at.elapsed() < lan::BROWSE_TIME,
        "stopped before the deadline"
    );
}

/// Real multicast on this host's interfaces: the machine's DNS-SD record is
/// browsed back with its bound port. Run explicitly with `--ignored` on a
/// host with multicast; a CI runner without multicast cannot run it.
#[test]
#[ignore = "requires multicast on a real interface"]
fn a_machine_advertises_dns_sd_that_a_device_browses() {
    let host_dir = TestDir::new("mdns-host");
    let port = free_port();
    let (host, _host_rx, _host_tx) = start_host(&host_dir, Some(port));
    let id: iroh::EndpointId = host.endpoint_id().parse().unwrap();
    let lookup = LanLookup::new(
        |_| Vec::new(),
        LanOptions {
            browse: Browse::DnsSd,
            multicast_lock: None,
        },
    );
    let started = Instant::now();
    let items = tcode_traverse::block_on(async {
        let mut items = Vec::new();
        let mut stream = lookup.resolve(id).unwrap();
        while let Some(item) = stream.next().await {
            let item = item.unwrap();
            assert_eq!(item.endpoint_id(), id);
            items.push((
                item.provenance(),
                item.endpoint_info().ip_addrs().copied().collect::<Vec<_>>(),
            ));
        }
        items
    });
    eprintln!("DNS-SD resolve took {:?}: {items:?}", started.elapsed());
    let browsed: Vec<SocketAddr> = items
        .iter()
        .filter(|(provenance, _)| *provenance == "lan-dns-sd")
        .flat_map(|(_, addrs)| addrs.clone())
        .collect();
    assert!(!browsed.is_empty(), "no DNS-SD result: {items:?}");
    assert!(browsed.iter().all(|addr| addr.port() == port));
    assert!(browsed.iter().all(|addr| !addr.ip().is_loopback()));
    assert!(
        started.elapsed() <= lan::BROWSE_TIME + Duration::from_secs(1),
        "the browse ends at its deadline"
    );
    host.shutdown();
}
