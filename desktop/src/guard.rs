use crate::tunnel::{Countries, Service};
use http::uri::Authority;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream as StdStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader,
    copy_bidirectional_with_sizes,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;

const HANDSHAKE: Duration = Duration::from_secs(8);
const HEADER_LIMIT: usize = 16 * 1024;
const COPY_BUFFER: usize = 32 * 1024;
const CLAUDE_DOMAINS: &str = include_str!("../../guard/assets/domains.txt");
const CHATGPT_DOMAINS: &str = include_str!("../assets/chatgpt-domains.txt");

fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}

fn denied() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}

fn in_zones(host: &str, zones: &str) -> bool {
    zones.lines().map(str::trim).any(|zone| {
        if zone.is_empty() || zone.starts_with('#') {
            return false;
        }
        if let Some(exact) = zone.strip_prefix('=') {
            return host == exact;
        }
        host == zone
            || host
                .strip_suffix(zone)
                .is_some_and(|part| part.ends_with('.'))
    })
}

fn service_at(host: &str) -> Option<Service> {
    if in_zones(host, CLAUDE_DOMAINS) {
        Some(Service::Claude)
    } else if in_zones(host, CHATGPT_DOMAINS) {
        Some(Service::ChatGpt)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revision(u64);

#[derive(Clone)]
struct Route {
    revision: Revision,
    country: String,
    socks: SocketAddr,
}

#[derive(Default)]
struct State {
    run: u64,
    revision: u64,
    countries: Countries,
    route: Option<Route>,
    active: HashMap<u64, (Service, StdStream)>,
    next_id: u64,
}

impl State {
    fn revoke(&mut self) {
        self.revision += 1;
        self.route = None;
        for (_, (_, stream)) in self.active.drain() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn route_for(&self, service: Service) -> Option<&Route> {
        let route = self.route.as_ref()?;
        (self.countries.get(service) == Some(route.country.as_str())).then_some(route)
    }
}

#[derive(Default)]
pub struct Gate(Mutex<State>);

impl Gate {
    pub fn begin(&self) -> u64 {
        let mut state = self.0.lock().unwrap();
        state.revoke();
        state.run += 1;
        state.run
    }

    pub fn revoke(&self, run: u64) -> Option<Revision> {
        let mut state = self.0.lock().unwrap();
        if state.run != run {
            return None;
        }
        state.revoke();
        Some(Revision(state.revision))
    }

    pub fn revision(&self, run: u64) -> Option<Revision> {
        let state = self.0.lock().unwrap();
        (state.run == run).then_some(Revision(state.revision))
    }

    pub fn configure(&self, countries: Countries) {
        let mut state = self.0.lock().unwrap();
        let previous = std::mem::replace(&mut state.countries, countries);
        let countries = state.countries.clone();
        state.active.retain(|_, (service, stream)| {
            if countries.get(*service) == previous.get(*service) {
                true
            } else {
                let _ = stream.shutdown(Shutdown::Both);
                false
            }
        });
    }

    pub fn approve(&self, run: u64, revision: Revision, country: String, socks: u16) -> bool {
        let mut state = self.0.lock().unwrap();
        if state.run != run || Revision(state.revision) != revision {
            return false;
        }
        state.route = Some(Route {
            revision,
            country,
            socks: (Ipv4Addr::LOCALHOST, socks).into(),
        });
        true
    }

    pub fn ready(&self, service: Service) -> bool {
        self.0.lock().unwrap().route_for(service).is_some()
    }

    pub fn verified(&self, revision: Revision) -> bool {
        self.0
            .lock()
            .unwrap()
            .route
            .as_ref()
            .is_some_and(|route| route.revision == revision)
    }

    fn route(&self, service: Service) -> Option<Route> {
        self.0.lock().unwrap().route_for(service).cloned()
    }

    fn track(
        self: &Arc<Self>,
        service: Service,
        revision: Revision,
        socket: &StdStream,
    ) -> io::Result<Permit> {
        let mut state = self.0.lock().unwrap();
        if state
            .route_for(service)
            .is_none_or(|route| route.revision != revision)
        {
            return Err(denied());
        }
        let id = state.next_id;
        state.next_id += 1;
        state.active.insert(id, (service, socket.try_clone()?));
        Ok(Permit {
            gate: Arc::clone(self),
            id,
        })
    }
}

struct Permit {
    gate: Arc<Gate>,
    id: u64,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.gate.0.lock().unwrap().active.remove(&self.id);
    }
}

struct Target {
    host: String,
    port: u16,
}

async fn header_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    budget: &mut usize,
) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    reader
        .take(*budget as u64)
        .read_until(b'\n', &mut line)
        .await?;
    *budget -= line.len();
    if !line.ends_with(b"\r\n") {
        return Err(invalid());
    }
    line.truncate(line.len() - 2);
    Ok(line)
}

async fn target(reader: &mut (impl AsyncBufRead + Unpin)) -> io::Result<Target> {
    let mut budget = HEADER_LIMIT;
    let request = header_line(reader, &mut budget).await?;
    let mut words = std::str::from_utf8(&request)
        .map_err(|_| invalid())?
        .split_ascii_whitespace();
    if words.next() != Some("CONNECT") {
        return Err(io::ErrorKind::Unsupported.into());
    }
    let authority: Authority = words
        .next()
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    let port = authority
        .port_u16()
        .filter(|port| *port != 0)
        .ok_or_else(invalid)?;
    if !matches!(words.next(), Some("HTTP/1.0" | "HTTP/1.1")) || words.next().is_some() {
        return Err(invalid());
    }
    while !header_line(reader, &mut budget).await?.is_empty() {}
    let host = authority
        .host()
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.is_empty()
        || host.len() > 253
        || (host.parse::<IpAddr>().is_err()
            && !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')))
    {
        return Err(invalid());
    }
    Ok(Target { host, port })
}

async fn socks_connect(stream: &mut TcpStream, target: &Target) -> io::Result<()> {
    stream.write_all(&[5, 1, 0]).await?;
    let mut selected = [0; 2];
    stream.read_exact(&mut selected).await?;
    if selected != [5, 0] {
        return Err(invalid());
    }
    let mut request = vec![5, 1, 0];
    match target.host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            request.push(1);
            request.extend(address.octets());
        }
        Ok(IpAddr::V6(address)) => {
            request.push(4);
            request.extend(address.octets());
        }
        Err(_) => {
            request.extend([3, target.host.len() as u8]);
            request.extend(target.host.as_bytes());
        }
    }
    request.extend(target.port.to_be_bytes());
    stream.write_all(&request).await?;
    let mut reply = [0; 4];
    stream.read_exact(&mut reply).await?;
    if reply[..3] != [5, 0, 0] {
        return Err(io::ErrorKind::ConnectionRefused.into());
    }
    let bytes = match reply[3] {
        1 => 4,
        4 => 16,
        3 => usize::from(stream.read_u8().await?),
        _ => return Err(invalid()),
    };
    let mut address = [0; 257];
    stream.read_exact(&mut address[..bytes + 2]).await?;
    Ok(())
}

async fn upstream(
    client: &mut BufReader<TcpStream>,
    gate: &Arc<Gate>,
    service: Service,
) -> io::Result<(TcpStream, Permit)> {
    let target = target(client).await?;
    if service_at(&target.host).is_some_and(|destination| destination != service) {
        return Err(denied());
    }
    let route = gate.route(service).ok_or_else(denied)?;
    let socket = TcpStream::connect(route.socks).await?.into_std()?;
    socket.set_nodelay(true)?;
    let permit = gate.track(service, route.revision, &socket)?;
    let mut socket = TcpStream::from_std(socket)?;
    socks_connect(&mut socket, &target).await?;
    Ok((socket, permit))
}

async fn relay(client: TcpStream, gate: Arc<Gate>, service: Service) -> io::Result<()> {
    client.set_nodelay(true)?;
    let mut client = BufReader::new(client);
    let result = timeout(HANDSHAKE, upstream(&mut client, &gate, service)).await;
    match result {
        Ok(Ok((mut socket, _permit))) => {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
            copy_bidirectional_with_sizes(&mut client, &mut socket, COPY_BUFFER, COPY_BUFFER)
                .await?;
        }
        result => {
            let response: &[u8] = match result {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Unsupported =>
                    b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData =>
                    b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                _ => b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            };
            client.write_all(response).await?;
        }
    }
    Ok(())
}

pub async fn serve(listener: TcpListener, gate: Arc<Gate>, service: Service) -> io::Result<()> {
    let slots = Arc::new(Semaphore::new(256));
    loop {
        let (socket, _) = listener.accept().await?;
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            continue;
        };
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let _slot = slot;
            let _ = relay(socket, gate, service).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_locks_share_one_proof_but_match_independently() {
        let gate = Gate::default();
        let run = gate.begin();
        gate.configure(Countries {
            claude: Some("AT".into()),
            chatgpt: Some("DE".into()),
        });
        let ticket = gate.revision(run).unwrap();
        assert!(gate.approve(run, ticket, "AT".into(), 1081));
        assert!(gate.ready(Service::Claude));
        assert!(!gate.ready(Service::ChatGpt));
        gate.configure(Countries {
            claude: Some("AT".into()),
            chatgpt: Some("AT".into()),
        });
        assert!(gate.ready(Service::ChatGpt));
        gate.revoke(run).unwrap();
        assert!(!gate.ready(Service::Claude));
        assert!(!gate.ready(Service::ChatGpt));
        assert!(!gate.approve(run, ticket, "AT".into(), 1081));
        assert!(gate.revoke(run - 1).is_none());
    }

    #[test]
    fn zone_boundaries_and_separate_services() {
        assert_eq!(
            service_at("files.oaiusercontent.com"),
            Some(Service::ChatGpt)
        );
        assert_eq!(service_at("api.anthropic.com"), Some(Service::Claude));
        assert_eq!(service_at("notopenai.com"), None);
        assert_eq!(service_at("claude.ai.example.com"), None);
    }

    #[tokio::test]
    async fn blocked_and_cross_service_requests_never_open_an_upstream_socket() {
        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let gate = Arc::new(Gate::default());
        let run = gate.begin();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve(listener, Arc::clone(&gate), Service::Claude));
        let request = |host| async move {
            let mut client = TcpStream::connect(address).await.unwrap();
            client
                .write_all(format!("CONNECT {host}:443 HTTP/1.1\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 503"));
        };
        request("claude.ai").await;
        gate.configure(Countries {
            claude: Some("AT".into()),
            chatgpt: Some("DE".into()),
        });
        assert!(gate.approve(
            run,
            gate.revision(run).unwrap(),
            "AT".into(),
            upstream.local_addr().unwrap().port()
        ));
        request("chatgpt.com").await;
        assert!(
            timeout(Duration::from_millis(40), upstream.accept())
                .await
                .is_err()
        );
        server.abort();
    }

    async fn fake_socks(listener: TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut greeting = [0; 3];
        stream.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 1, 0]);
        stream.write_all(&[5, 0]).await.unwrap();
        let mut request = [0; 16];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"\x05\x01\x00\x03\tclaude.ai\x01\xbb");
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
            .await
            .unwrap();
        stream
    }

    async fn connect_through_guard(socks: u16) -> (TcpStream, Arc<Gate>, u64) {
        let gate = Arc::new(Gate::default());
        let run = gate.begin();
        gate.configure(Countries {
            claude: Some("AT".into()),
            chatgpt: Some("DE".into()),
        });
        assert!(gate.approve(run, gate.revision(run).unwrap(), "AT".into(), socks));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let worker = Arc::clone(&gate);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = relay(stream, worker, Service::Claude).await;
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"CONNECT claude.ai:443 HTTP/1.1\r\nHost: claude.ai:443\r\n\r\n")
            .await
            .unwrap();
        let mut response = [0; b"HTTP/1.1 200 Connection Established\r\n\r\n".len()];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        (client, gate, run)
    }

    #[tokio::test]
    async fn reconnect_closes_existing_socks_stream() {
        timeout(Duration::from_secs(4), async {
            let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = upstream.local_addr().unwrap().port();
            let ((mut client, gate, run), mut server) =
                tokio::join!(connect_through_guard(port), fake_socks(upstream));
            client.write_all(b"hello").await.unwrap();
            let mut data = [0; 5];
            server.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"hello");
            let old = gate.revision(run).unwrap();
            gate.revoke(run).unwrap();
            let read = server.read(&mut data).await;
            assert!(matches!(read, Ok(0)) || read.is_err());
            assert!(!gate.approve(run, old, "AT".into(), port));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "local throughput measurement"]
    async fn loopback_relay_throughput() {
        static CHUNK: [u8; 32 * 1024] = [31; 32 * 1024];
        const CHUNKS: usize = 4_096;
        let mut results = [Vec::new(), Vec::new()];
        for _ in 0..7 {
            for (index, measured) in results.iter_mut().enumerate() {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                let address = listener.local_addr().unwrap();
                let (mut source, mut sink) = if index == 0 {
                    let (client, server) =
                        tokio::join!(TcpStream::connect(address), listener.accept());
                    (server.unwrap().0, client.unwrap())
                } else {
                    let (client, server) =
                        tokio::join!(connect_through_guard(address.port()), fake_socks(listener));
                    (server, client.0)
                };
                let start = std::time::Instant::now();
                let ((), received) = tokio::join!(
                    async {
                        for _ in 0..CHUNKS {
                            source.write_all(&CHUNK).await.unwrap();
                        }
                        source.shutdown().await.unwrap();
                    },
                    async {
                        tokio::io::copy(&mut sink, &mut tokio::io::sink())
                            .await
                            .unwrap()
                    }
                );
                assert_eq!(received, (CHUNKS * CHUNK.len()) as u64);
                measured
                    .push(received as f64 * 8.0 / start.elapsed().as_secs_f64() / 1_000_000_000.0);
            }
        }
        for (label, samples) in ["raw TCP", "PsiMole relay"]
            .into_iter()
            .zip(results.iter_mut())
        {
            samples.sort_by(f64::total_cmp);
            println!(
                "{label}: {:.2} Gbit/s median, 128 MiB over loopback",
                samples[samples.len() / 2]
            );
        }
    }
}
