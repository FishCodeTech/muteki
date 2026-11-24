use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
// The agent's documented TLS/DNS interfaces route real HTTPS requests into a loopback
// HTTP fixture without a certificate dependency or an endpoint override in production.
struct LoopbackTls;
impl ureq::TlsConnector for LoopbackTls {
    fn connect(
        &self,
        _: &str,
        io: Box<dyn ureq::ReadWrite>,
    ) -> Result<Box<dyn ureq::ReadWrite>, ureq::Error> {
        Ok(io)
    }
}
pub(crate) struct Exchange {
    pub(crate) request: String,
    pub(crate) body: Vec<u8>,
    pub(crate) stream: TcpStream,
}
impl Exchange {
    /// A reply whose `headers` declare a transfer encoding carries no length of its own.
    pub(crate) fn reply(mut self, status: u16, headers: &str, body: &[u8]) {
        let length = if headers.to_ascii_lowercase().contains("transfer-encoding:") {
            String::new()
        } else {
            format!("Content-Length: {}\r\n", body.len())
        };
        write!(
            self.stream,
            "HTTP/1.1 {status} Fixture\r\n{length}Connection: close\r\n{headers}\r\n"
        )
        .unwrap();
        let _ = self.stream.write_all(body);
    }
}
pub(crate) struct Fixture {
    pub(crate) address: std::net::SocketAddr,
    pub(crate) incoming: mpsc::Receiver<Exchange>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, incoming) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            'connections: for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        continue 'connections;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let length = request
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                if tx
                    .send(Exchange {
                        request,
                        body,
                        stream,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            address,
            incoming,
            stop,
            thread: Some(thread),
        }
    }
    pub(crate) fn builder(&self) -> ureq::AgentBuilder {
        let address = self.address;
        ureq::AgentBuilder::new()
            .resolver(move |_: &str| Ok(vec![address]))
            .tls_connector(Arc::new(LoopbackTls))
    }
    pub(crate) fn next(&self) -> Exchange {
        self.incoming
            .recv_timeout(Duration::from_secs(5))
            .expect("client sent request to fixture")
    }
    pub(crate) fn call<T: Send>(
        &self,
        run: impl FnOnce() -> T + Send,
        status: u16,
        headers: &str,
        body: &[u8],
    ) -> (T, String, Vec<u8>) {
        thread::scope(|scope| {
            let job = scope.spawn(run);
            let exchange = self.next();
            let request = exchange.request.clone();
            let sent = exchange.body.clone();
            exchange.reply(status, headers, body);
            (job.join().unwrap(), request, sent)
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        self.thread.take().unwrap().join().unwrap();
    }
}
