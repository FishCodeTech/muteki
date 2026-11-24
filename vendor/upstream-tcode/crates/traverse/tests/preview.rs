//! The browser-side Preview adapters over real tunnels: a machine and a
//! paired device in one process over loopback, local TCP fixtures standing
//! in for the machine's dev servers.
use std::{
    io::{Read, Write as _},
    net::{Shutdown, TcpListener, TcpStream},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use tcode_client::{ConnectionState, host::Transport, host::TunnelOpener, pairing::PairedHost};
use tcode_traverse::{
    DeviceIdentity, HostConfig, HostMux, TraverseHost,
    preview::{NativeProxy, PreviewEndpoint, PreviewRoutes},
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tcode-preview-{tag}-{}-{}",
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

/// A machine, a device paired with it and the device's attachment.
struct Machine {
    host: Option<TraverseHost>,
    _backend: (
        async_channel::Receiver<tcode_client::attachment::AttachmentMessage>,
        async_channel::Sender<String>,
    ),
    paired: PairedHost,
    _transport: Transport,
    tunnels: Arc<dyn TunnelOpener>,
    _dirs: [TestDir; 2],
}

impl Machine {
    fn start() -> Self {
        let host_dir = TestDir::new("host");
        let (to_host, host_rx) =
            async_channel::unbounded::<tcode_client::attachment::AttachmentMessage>();
        let (host_tx, from_host) = async_channel::unbounded::<String>();
        let host = TraverseHost::start(
            HostMux::new(to_host, from_host),
            HostConfig {
                host_name: "Preview Host".into(),
                data_dir: host_dir.0.clone(),
                traverse: Vec::new(),
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .unwrap();
        let device_dir = TestDir::new("device");
        let device = DeviceIdentity::load_or_create(&device_dir.0).unwrap();
        device.set_details("laptop".into(), None);
        let minted = host.new_invitation();
        let paired = tcode_traverse::pair_blocking(
            &minted.invite,
            Some(std::net::Ipv4Addr::LOCALHOST.into()),
            &device,
        )
        .unwrap();
        let transport = tcode_traverse::connect(&paired, &device);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(Instant::now() < deadline, "attachment never connected");
            if let Ok(ConnectionState::Syncing { .. }) = transport.state.try_recv() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let tunnels = transport.current_host.as_ref().unwrap().tunnels().unwrap();
        Self {
            host: Some(host),
            _backend: (host_rx, host_tx),
            paired,
            _transport: transport,
            tunnels,
            _dirs: [host_dir, device_dir],
        }
    }

    fn endpoint(&self) -> PreviewEndpoint {
        PreviewEndpoint::new(&self.paired, self.tunnels.clone())
    }

    fn proxy(&self) -> (NativeProxy, String) {
        let proxy = NativeProxy::new(self.endpoint()).unwrap();
        let address = proxy.origin().trim_start_matches("http://").to_owned();
        (proxy, address)
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if let Some(host) = self.host.take() {
            host.shutdown();
        }
    }
}

fn socket(address: &str) -> TcpStream {
    let socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    socket
}

fn head(stream: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() <= 16384);
    }
    String::from_utf8(bytes).unwrap()
}

#[test]
fn absolute_form_requests_reach_the_origin_in_origin_form_without_proxy_headers() {
    let machine = Machine::start();
    let (_proxy, address) = machine.proxy();
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = origin.local_addr().unwrap().port();
    let origin = std::thread::spawn(move || {
        let (mut stream, _) = origin.accept().unwrap();
        let request = head(&mut stream);
        assert!(
            request.starts_with("GET /page?source=host HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(request.contains(&format!("\r\nHost: localhost:{port}\r\n")));
        assert!(request.contains("\r\nConnection: close\r\n"));
        assert!(request.contains("\r\naccept: text/html\r\n"));
        let lowered = request.to_ascii_lowercase();
        assert!(!lowered.contains("proxy-"), "{request}");
        assert!(!lowered.contains("keep-alive"), "{request}");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nserved on host",
            )
            .unwrap();
    });
    let mut browser = socket(&address);
    browser.write_all(format!("GET http://localhost:{port}/page?source=host HTTP/1.1\r\nHost: wrong.example\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: Basic dGNvZGU6c2VjcmV0\r\nAccept: text/html\r\n\r\n").as_bytes()).unwrap();
    let mut response = String::new();
    browser.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with("served on host"));
    origin.join().unwrap();
}

#[test]
fn pipelined_requests_and_chunk_trailers_never_reach_the_origin_and_bad_framing_is_400() {
    let machine = Machine::start();
    let (_proxy, address) = machine.proxy();
    for chunked in [false, true] {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = origin.local_addr().unwrap().port();
        let origin = std::thread::spawn(move || {
            let (mut stream, _) = origin.accept().unwrap();
            head(&mut stream);
            let expected = if chunked {
                b"3\r\nabc\r\n0\r\n\r\n".as_slice()
            } else {
                b"abc".as_slice()
            };
            let mut body = vec![0; expected.len()];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(body, expected);
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
            let mut extra = Vec::new();
            stream.read_to_end(&mut extra).unwrap();
            assert!(
                extra.is_empty(),
                "pipelined bytes leaked to origin: {extra:?}"
            );
        });
        let framing = if chunked {
            "Transfer-Encoding: chunked"
        } else {
            "Content-Length: 3"
        };
        let body = if chunked {
            "3\r\nabc\r\n0\r\nX-Smuggled: trailer\r\n\r\n"
        } else {
            "abc"
        };
        let mut browser = socket(&address);
        browser
            .write_all(
                format!(
                    "POST http://localhost:{port}/ HTTP/1.1\r\n{framing}\r\n\r\n{body}GET http://evil/ HTTP/1.1\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
        assert!(head(&mut browser).starts_with("HTTP/1.1 200 OK"));
        // A reset instead of an orderly close discards the response on Windows.
        assert!(browser.read_to_end(&mut Vec::new()).is_ok());
        origin.join().unwrap();
    }
    for request in [
        "POST http://localhost:1/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 3\r\n\r\n",
        "POST http://localhost:1/ HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n",
        "GET /origin-form HTTP/1.1\r\nHost: localhost:1\r\n\r\n",
        "CONNECT localhost:1/path HTTP/1.1\r\n\r\n",
        "GET http://user:secret@localhost:1/ HTTP/1.1\r\n\r\n",
    ] {
        let mut browser = socket(&address);
        browser.write_all(request.as_bytes()).unwrap();
        assert!(
            head(&mut browser).starts_with("HTTP/1.1 400 "),
            "{request:?} was not rejected"
        );
    }
}

#[test]
fn connect_tunnels_preserve_half_close_and_die_with_the_proxy() {
    let machine = Machine::start();
    let (proxy, address) = machine.proxy();
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let destination = origin.local_addr().unwrap();
    let origin = std::thread::spawn(move || {
        let (mut stream, _) = origin.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(b"response first").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut request = String::new();
        stream.read_to_string(&mut request).unwrap();
        assert_eq!(request, "request after remote EOF");
    });
    let mut browser = socket(&address);
    browser
        .write_all(
            format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n\r\n").as_bytes(),
        )
        .unwrap();
    assert!(head(&mut browser).starts_with("HTTP/1.1 200 "));
    let mut reply = String::new();
    browser.read_to_string(&mut reply).unwrap();
    assert_eq!(reply, "response first");
    browser.write_all(b"request after remote EOF").unwrap();
    browser.shutdown(Shutdown::Write).unwrap();
    origin.join().unwrap();

    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let destination = origin.local_addr().unwrap();
    let mut browser = socket(&address);
    browser
        .write_all(format!("CONNECT {destination} HTTP/1.1\r\n\r\n").as_bytes())
        .unwrap();
    assert!(head(&mut browser).starts_with("HTTP/1.1 200 "));
    let (mut target, _) = origin.accept().unwrap();
    target
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    browser.write_all(b"\x16\x03\x01tls bytes").unwrap();
    let mut seen = [0; 12];
    target.read_exact(&mut seen).unwrap();
    assert_eq!(&seen, b"\x16\x03\x01tls bytes");
    let mut pending = socket(&address);
    pending.write_all(b"GET ").unwrap();
    drop(proxy);
    for socket in [&mut browser, &mut pending] {
        match socket.read(&mut [0]) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            result => panic!("the proxy did not close its connection: {result:?}"),
        }
    }
    assert_eq!(
        target.read(&mut [0]).unwrap(),
        0,
        "the tunnel outlived the proxy"
    );
}

#[test]
fn websocket_upgrades_pass_through_an_absolute_form_request() {
    let machine = Machine::start();
    let (_proxy, address) = machine.proxy();
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = origin.local_addr().unwrap().port();
    let origin = std::thread::spawn(move || {
        let (mut stream, _) = origin.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let request = head(&mut stream);
        assert!(request.starts_with("GET /ws HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("\r\nConnection: Upgrade\r\n"), "{request}");
        assert!(request.contains("\r\nupgrade: websocket\r\n"), "{request}");
        assert!(
            request.contains("\r\nsec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n"),
            "{request}"
        );
        stream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .unwrap();
        let mut frame = [0; 5];
        stream.read_exact(&mut frame).unwrap();
        assert_eq!(&frame, b"\x81\x03abc");
        stream.write_all(b"\x81\x03xyz").unwrap();
        let mut close = [0; 2];
        stream.read_exact(&mut close).unwrap();
        assert_eq!(&close, b"\x88\x00");
        stream.shutdown(Shutdown::Write).unwrap();
    });
    let mut browser = socket(&address);
    browser.write_all(format!("GET http://localhost:{port}/ws HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").as_bytes()).unwrap();
    assert!(head(&mut browser).starts_with("HTTP/1.1 101 "));
    browser.write_all(b"\x81\x03abc").unwrap();
    let mut frame = [0; 5];
    browser.read_exact(&mut frame).unwrap();
    assert_eq!(&frame, b"\x81\x03xyz");
    browser.write_all(b"\x88\x00").unwrap();
    assert_eq!(browser.read(&mut [0]).unwrap(), 0);
    origin.join().unwrap();
}

#[test]
fn unreachable_targets_answer_502_from_the_proxy() {
    let machine = Machine::start();
    let (_proxy, address) = machine.proxy();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut browser = socket(&address);
    browser
        .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
        .unwrap();
    let mut response = String::new();
    browser.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 502 "), "{response}");
    assert!(
        response.ends_with(&format!(
            "Preview Host could not connect to 127.0.0.1:{port}: connection refused"
        )),
        "{response}"
    );
}

#[test]
fn mapped_loopback_connections_become_tunnels_and_close_with_the_slot() {
    let machine = Machine::start();
    // The viewing machine already occupies the remote port: the mapped
    // socket is allocated elsewhere and the logical URL survives.
    let destination = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = destination.local_addr().unwrap().port();
    let intent = format!("http://localhost:{port}/page?source=remote#fragment");
    let mut routes = PreviewRoutes::new(machine.endpoint());
    let actual = routes.navigate(&intent).unwrap();
    let actual_url = url::Url::parse(&actual).unwrap();
    assert_ne!(actual_url.port().unwrap(), port);
    assert_eq!(routes.logical_url(&actual), intent);
    assert_eq!(routes.navigation(&actual).unwrap(), actual);
    assert_eq!(
        routes.external_url(&intent).as_deref(),
        Some(actual.as_str())
    );
    let mut browser = socket(&format!("127.0.0.1:{}", actual_url.port().unwrap()));
    // Opaque HTTP/WS-like bytes, including site authorization and binary
    // bytes: the mapper must not parse, rewrite or remove any of these.
    let bytes = b"GET /page?source=remote HTTP/1.1\r\nHost: localhost:12345\r\nAuthorization: Basic site-only\r\nUpgrade: websocket\r\n\r\n\x00\xff\x81payload";
    browser.write_all(bytes).unwrap();
    let (mut remote, _) = destination.accept().unwrap();
    remote
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut received = vec![0; bytes.len()];
    remote.read_exact(&mut received).unwrap();
    assert_eq!(received, bytes);
    browser.shutdown(Shutdown::Write).unwrap();
    assert_eq!(remote.read(&mut [0]).unwrap(), 0);
    remote.write_all(b"remote after EOF").unwrap();
    let mut reply = [0; 16];
    browser.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"remote after EOF");
    assert_eq!(routes.error(), None);

    // An IPv6 literal keeps its address on the machine side.
    let six = TcpListener::bind("[::1]:0").unwrap();
    let six_port = six.local_addr().unwrap().port();
    let mapped = routes
        .navigate(&format!("http://[::1]:{six_port}/"))
        .unwrap();
    let mut browser_six = socket(&format!(
        "[::1]:{}",
        url::Url::parse(&mapped).unwrap().port().unwrap()
    ));
    browser_six.write_all(b"six").unwrap();
    let (mut remote_six, _) = six.accept().unwrap();
    let mut seen = [0; 3];
    remote_six.read_exact(&mut seen).unwrap();
    assert_eq!(&seen, b"six");

    // A closed port reports through the route's error surface.
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let changes = routes.changes();
    let mapped = routes
        .navigate(&format!("http://127.0.0.1:{closed}/"))
        .unwrap();
    let mut failing = socket(&format!(
        "127.0.0.1:{}",
        url::Url::parse(&mapped).unwrap().port().unwrap()
    ));
    assert_eq!(failing.read(&mut [0]).unwrap(), 0);
    changes.recv_blocking().unwrap();
    assert_eq!(
        routes.error().as_deref(),
        Some(
            format!("Preview Host could not connect to 127.0.0.1:{closed}: connection refused")
                .as_str()
        )
    );

    drop(routes);
    assert_eq!(
        browser.read(&mut [0]).unwrap(),
        0,
        "slot close must close accepted tunnels"
    );
    assert_eq!(remote.read(&mut [0]).unwrap(), 0);
    assert_eq!(remote_six.read(&mut [0]).unwrap(), 0);
    assert!(
        TcpStream::connect(("127.0.0.1", actual_url.port().unwrap())).is_err(),
        "slot close must drop listeners"
    );
}
