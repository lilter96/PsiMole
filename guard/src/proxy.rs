use crate::gate::{Gate, Permit};
use crate::web;
use bytes::Bytes;
use http::uri::Authority;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::body::Incoming;
use hyper::header::{CONNECTION, HOST, HeaderMap};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use rustls::server::Acceptor;
use std::convert::Infallible;
use std::env;
use std::fs;
use std::io::{self, Cursor};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr as LocalAddr, UnixDatagram};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream, lookup_host};
use tokio::time::timeout;

const DOMAINS: &str = "/etc/claude-at/domains.txt";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const HELLO_TIMEOUT: Duration = Duration::from_secs(4);

fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}

fn blocked() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}

#[derive(Clone, Copy)]
enum Inbound {
    Http { strict: bool },
    Socks,
    Direct,
}

#[derive(Clone, Debug)]
struct Target {
    host: String,
    port: u16,
}

impl Target {
    fn new(host: &str, port: u16) -> io::Result<Self> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(Self::from(SocketAddr::new(ip, port)));
        }
        if port == 0
            || host.is_empty()
            || host.len() > 253
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(invalid());
        }
        Ok(Self { host, port })
    }

    fn authority(input: &str, fallback: u16) -> io::Result<Self> {
        let value = input.parse::<Authority>().map_err(|_| invalid())?;
        let host = value.host();
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        Self::new(host, value.port_u16().unwrap_or(fallback))
    }

    fn ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }

    fn synthetic(&self) -> bool {
        self.ip() == Some(IpAddr::V4(web::CLAUDE_DNS_ALIAS))
    }
}

impl From<SocketAddr> for Target {
    fn from(addr: SocketAddr) -> Self {
        Self {
            host: addr.ip().to_canonical().to_string(),
            port: addr.port(),
        }
    }
}

struct Proxy {
    gate: Arc<Gate>,
    domains: Vec<String>,
}

fn in_domain(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

async fn name_matches_peer(target: &Target, name: &str) -> bool {
    let Some(ip) = target.ip() else {
        return false;
    };
    match timeout(HELLO_TIMEOUT, lookup_host((name, target.port))).await {
        Ok(Ok(mut addresses)) => addresses.any(|address| address.ip().to_canonical() == ip),
        _ => false,
    }
}

impl Proxy {
    fn protected(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.domains.iter().any(|domain| {
            if let Some(exact) = domain.strip_prefix('=') {
                host == exact
            } else {
                in_domain(&host, domain)
            }
        })
    }

    fn protected_target(&self, target: &Target) -> bool {
        if let Some(address) = target.ip() {
            return web::anthropic_ip(address);
        }
        self.protected(&target.host)
    }

    fn proxy_requires_country(&self, target: &Target) -> bool {
        self.protected_target(target) || (!web::is_web_port(target.port) && target.ip().is_some())
    }

    async fn dial(
        &self,
        target: &Target,
        proxy: bool,
        austria: bool,
    ) -> io::Result<(TcpStream, Option<Permit>)> {
        timeout(CONNECT_TIMEOUT, async {
            if target.synthetic() {
                return Err(blocked());
            }
            if !proxy && !austria {
                let addr = SocketAddr::new(target.ip().ok_or_else(invalid)?, target.port);
                return Ok((TcpStream::connect(addr).await?, None));
            }
            let before = if austria {
                Some(self.gate.ready().ok_or_else(blocked)?)
            } else {
                None
            };
            let mut upstream = TcpStream::connect(self.gate.socks).await?;
            let permit = if let Some(before) = before {
                let stream = upstream.into_std()?;
                let permit = self.gate.track(&stream, &before)?;
                upstream = TcpStream::from_std(stream)?;
                Some(permit)
            } else {
                None
            };
            socks_connect(&mut upstream, target).await?;
            Ok((upstream, permit))
        })
        .await?
    }

    async fn stream(
        &self,
        mut client: impl AsyncRead + AsyncWrite + Unpin,
        mut target: Target,
        proxy: bool,
    ) -> io::Result<()> {
        let mut austria = self.protected_target(&target);
        let mut prefix = Vec::new();
        if !austria && web::is_web_port(target.port) {
            let name = timeout(HELLO_TIMEOUT, request_host(&mut client, &mut prefix)).await??;
            if let Some(announced) = name {
                let announced_target = Target::new(&announced.name, target.port)?;
                let numeric_name = announced_target.ip().is_some();
                let name = announced_target.host;
                let named = self.protected(&name);
                // ECH can advertise an unrelated outer name. For an explicit
                // proxy request carrying the same DNS name, that name is also
                // available independently of TLS (including ECH GREASE).
                let shared_name =
                    matches!(name.as_str(), "cloudflare-ech.com" | "cloudflare-ech.net");
                let hidden = !named
                    && !numeric_name
                    && announced.ech
                    && (shared_name
                        || if proxy {
                            target.ip().is_some() || target.host != name
                        } else {
                            target.ip().is_some_and(web::shared_web_edge)
                                || !name_matches_peer(&target, &name).await
                        });
                austria = named || numeric_name || hidden;
                if target.synthetic() {
                    if !named {
                        return Err(blocked());
                    }
                    target = Target::new(&name, target.port)?;
                }
            } else {
                // An IP with no credible hostname could belong to a shared
                // CDN. A missing hostname cannot authorize a direct route.
                austria = true;
            }
        }
        let (mut remote, _permit) = self.dial(&target, proxy, austria).await?;
        remote.write_all(&prefix).await?;
        copy_bidirectional(&mut client, &mut remote).await?;
        Ok(())
    }

    async fn redirected(self: &Arc<Self>, mut client: TcpStream) -> io::Result<()> {
        let address = original_destination(&client)?;
        if address == client.local_addr()? {
            return Err(invalid());
        }
        let target = Target::from(address);
        if !web::is_web_port(address.port()) {
            return self.stream(client, target, false).await;
        }
        let mut prefix = Vec::new();
        let protocol =
            timeout(HELLO_TIMEOUT, connection_protocol(&mut client, &mut prefix)).await??;
        let (reader, writer) = tokio::io::split(client);
        let mut client = tokio::io::join(Cursor::new(prefix).chain(reader), writer);
        match protocol {
            Protocol::Http => self.serve_http(client, false, Some(address)).await,
            Protocol::Tls => self.stream(client, target, false).await?,
            Protocol::Other => {
                // A port number alone does not make a binary stream HTTP.
                let protected =
                    self.protected_target(&target) || web::shared_web_edge(address.ip());
                let (mut remote, _permit) = self.dial(&target, false, protected).await?;
                copy_bidirectional(&mut client, &mut remote).await?;
            }
        }
        Ok(())
    }

    async fn serve_socks(&self, mut client: TcpStream) -> io::Result<()> {
        let target = timeout(HELLO_TIMEOUT, socks_request(&mut client)).await??;
        let austria = self.proxy_requires_country(&target);
        if austria || !web::is_web_port(target.port) {
            let (mut remote, _permit) = match self.dial(&target, true, austria).await {
                Ok(remote) => remote,
                Err(err) => {
                    client.write_all(socks_reply(2)).await?;
                    return Err(err);
                }
            };
            client.write_all(socks_reply(0)).await?;
            copy_bidirectional(&mut client, &mut remote).await?;
            return Ok(());
        }
        // The browser may have resolved the name locally before SOCKS.
        // Inspect its first request before sending any destination to Psiphon.
        client.write_all(socks_reply(0)).await?;
        self.stream(client, target, true).await
    }

    async fn http(
        self: &Arc<Self>,
        mut req: Request<Incoming>,
        strict: bool,
        original: Option<SocketAddr>,
    ) -> io::Result<Response<Body>> {
        if req.headers().get_all(HOST).iter().count() > 1 {
            return Err(invalid());
        }
        let uri_host = req.uri().authority().map(Authority::as_str);
        let header_host = req.headers().get(HOST).and_then(|host| host.to_str().ok());
        if req.method() == Method::CONNECT {
            if original.is_some() {
                return Err(invalid());
            }
            let target = Target::authority(uri_host.ok_or_else(invalid)?, 443)?;
            let austria = strict || self.proxy_requires_country(&target);
            let upgrade = hyper::upgrade::on(&mut req);
            if austria || !web::is_web_port(target.port) {
                let (mut remote, permit) = self.dial(&target, true, austria).await?;
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Ok(stream) = upgrade.await {
                        let _ = copy_bidirectional(&mut TokioIo::new(stream), &mut remote).await;
                    }
                });
            } else {
                let proxy = Arc::clone(self);
                tokio::spawn(async move {
                    if let Ok(stream) = upgrade.await {
                        let _ = proxy.stream(TokioIo::new(stream), target, true).await;
                    }
                });
            }
            return Ok(response(StatusCode::OK, ""));
        }
        let authority = uri_host.or(header_host).ok_or_else(invalid)?.to_owned();
        let mut target = Target::authority(&authority, 80)?;
        let austria = strict
            || self.protected_target(&target)
            || target.ip().is_some()
            || header_host
                .and_then(|value| Target::authority(value, 80).ok())
                .is_some_and(|host| self.protected_target(&host));
        if let Some(address) = original {
            if !austria {
                target = Target::from(address);
            } else if target.ip().is_none() {
                target.port = address.port();
            }
        }
        let austria = austria || self.protected_target(&target);
        let (upstream, permit) = self.dial(&target, original.is_none(), austria).await?;
        strip_hop_headers(req.headers_mut());
        let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
        *req.uri_mut() = path.parse::<Uri>().map_err(|_| invalid())?;
        if !req.headers().contains_key(HOST) {
            req.headers_mut()
                .insert(HOST, authority.parse().map_err(|_| invalid())?);
        }
        req.headers_mut()
            .insert(CONNECTION, "close".parse().unwrap());
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(upstream))
                .await
                .map_err(io::Error::other)?;
        tokio::spawn(async move {
            let _permit = permit;
            let _ = connection.await;
        });
        let mut res = sender.send_request(req).await.map_err(io::Error::other)?;
        strip_hop_headers(res.headers_mut());
        Ok(res.map(BodyExt::boxed_unsync))
    }

    async fn serve_http(
        self: &Arc<Self>,
        stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
        strict: bool,
        original: Option<SocketAddr>,
    ) {
        let service = service_fn(|req| async move {
            let res = match self.http(req, strict, original).await {
                Ok(res) => res,
                Err(err) if err.kind() == io::ErrorKind::PermissionDenied => response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Claude requires a verified Psiphon connection to its selected country.\n",
                ),
                Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                    response(StatusCode::BAD_REQUEST, "")
                }
                Err(_) => response(StatusCode::BAD_GATEWAY, ""),
            };
            Ok::<_, Infallible>(res)
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(Duration::from_secs(12))
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades()
            .await;
    }
}

type Body = UnsyncBoxBody<Bytes, hyper::Error>;

fn response(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from_static(message.as_bytes()))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let names: Vec<_> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(|s| s.trim().parse::<hyper::header::HeaderName>().ok())
        .collect();
    for name in names {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authorization",
        "proxy-authenticate",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

fn socks_reply(code: u8) -> &'static [u8; 10] {
    match code {
        0 => b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00",
        _ => b"\x05\x02\x00\x01\x00\x00\x00\x00\x00\x00",
    }
}

async fn socks_request(stream: &mut TcpStream) -> io::Result<Target> {
    let mut hello = [0; 2];
    stream.read_exact(&mut hello).await?;
    if hello[0] != 5 || hello[1] == 0 {
        return Err(invalid());
    }
    let mut methods = vec![0; usize::from(hello[1])];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        stream.write_all(&[5, 255]).await?;
        return Err(invalid());
    }
    stream.write_all(&[5, 0]).await?;
    let mut header = [0; 4];
    stream.read_exact(&mut header).await?;
    if header[..3] != [5, 1, 0] {
        stream.write_all(socks_reply(2)).await?;
        return Err(invalid());
    }
    let host = socks_address(stream, header[3]).await?;
    Target::new(&host, stream.read_u16().await?)
}

async fn socks_address(stream: &mut TcpStream, kind: u8) -> io::Result<String> {
    match kind {
        1 => {
            let mut address = [0; 4];
            stream.read_exact(&mut address).await?;
            Ok(Ipv4Addr::from(address).to_string())
        }
        4 => {
            let mut address = [0; 16];
            stream.read_exact(&mut address).await?;
            Ok(Ipv6Addr::from(address).to_string())
        }
        3 => {
            let mut name = vec![0; usize::from(stream.read_u8().await?)];
            stream.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| invalid())
        }
        _ => Err(invalid()),
    }
}

async fn socks_connect(stream: &mut TcpStream, target: &Target) -> io::Result<()> {
    stream.write_all(&[5, 1, 0]).await?;
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).await?;
    if reply != [5, 0] {
        return Err(invalid());
    }
    let mut request = vec![5, 1, 0];
    match target.ip() {
        Some(IpAddr::V4(addr)) => {
            request.push(1);
            request.extend(addr.octets());
        }
        Some(IpAddr::V6(addr)) => {
            request.push(4);
            request.extend(addr.octets());
        }
        None => {
            request.extend([3, target.host.len() as u8]);
            request.extend_from_slice(target.host.as_bytes());
        }
    }
    request.extend(target.port.to_be_bytes());
    stream.write_all(&request).await?;
    let mut reply = [0; 4];
    stream.read_exact(&mut reply).await?;
    if reply[..3] != [5, 0, 0] {
        return Err(io::ErrorKind::ConnectionRefused.into());
    }
    socks_address(stream, reply[3]).await?;
    stream.read_u16().await?;
    Ok(())
}

struct AnnouncedHost {
    name: String,
    ech: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Http,
    Tls,
    Other,
}

async fn connection_protocol(
    reader: &mut (impl AsyncRead + Unpin),
    prefix: &mut Vec<u8>,
) -> io::Result<Protocol> {
    let first = reader.read_u8().await?;
    prefix.push(first);
    if first == 22 {
        let mut version = [0; 2];
        reader.read_exact(&mut version).await?;
        prefix.extend(version);
        return Ok(if version[0] == 3 && version[1] <= 4 {
            Protocol::Tls
        } else {
            Protocol::Other
        });
    }
    if matches!(first, b'\r' | b'\n' | b'\t' | b' ') {
        return Ok(Protocol::Http);
    }
    let mut byte = first;
    // A request starts with an RFC 9110 method token and a space.
    // Long method tokens are left to the HTTP parser.
    while prefix.len() <= 32 {
        if byte == b' ' {
            return Ok(Protocol::Http);
        }
        if !byte.is_ascii_alphanumeric() && !b"!#$%&'*+-.^`_|~".contains(&byte) {
            return Ok(Protocol::Other);
        }
        byte = reader.read_u8().await?;
        prefix.push(byte);
    }
    Ok(Protocol::Http)
}

async fn request_host(
    reader: &mut (impl AsyncRead + Unpin),
    prefix: &mut Vec<u8>,
) -> io::Result<Option<AnnouncedHost>> {
    let first = reader.read_u8().await?;
    if first != 22 {
        prefix.push(first);
        let mut chunk = [0; 1024];
        while prefix.len() < 16 * 1024 {
            let length = reader.read(&mut chunk).await?;
            if length == 0 {
                return Err(invalid());
            }
            prefix.extend_from_slice(&chunk[..length]);
            let mut headers = [httparse::EMPTY_HEADER; 128];
            let mut request = httparse::Request::new(&mut headers);
            match request.parse(prefix) {
                Ok(httparse::Status::Complete(_)) => {
                    for header in request.headers {
                        if header.name.eq_ignore_ascii_case("host") {
                            let value = std::str::from_utf8(header.value).map_err(|_| invalid())?;
                            let name = Target::authority(value.trim(), 80)?.host;
                            return Ok(Some(AnnouncedHost { name, ech: false }));
                        }
                    }
                    return Ok(None);
                }
                Ok(httparse::Status::Partial) => {}
                Err(_) => return Ok(None),
            }
        }
        return Err(invalid());
    }

    let mut hello = Acceptor::default();
    let mut kind = first;
    while prefix.len() < 128 * 1024 {
        let mut header = [kind, 0, 0, 0, 0];
        reader.read_exact(&mut header[1..]).await?;
        let size = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if size > 18 * 1024 {
            return Err(invalid());
        }
        let record = prefix.len();
        prefix.extend(header);
        let start = prefix.len();
        prefix.resize(start + size, 0);
        reader.read_exact(&mut prefix[start..]).await?;
        let mut bytes = Cursor::new(&prefix[record..]);
        hello.read_tls(&mut bytes)?;
        match hello.accept() {
            Ok(Some(accepted)) => {
                return Ok(accepted
                    .client_hello()
                    .server_name()
                    .map(|name| AnnouncedHost {
                        name: name.to_owned(),
                        ech: web::has_ech(prefix).unwrap_or(true),
                    }));
            }
            Ok(None) => kind = reader.read_u8().await?,
            Err(_) => return Err(invalid()),
        }
    }
    Err(invalid())
}

fn original_destination(socket: &TcpStream) -> io::Result<SocketAddr> {
    const ORIGINAL_DST: libc::c_int = 80;
    // Netfilter stores the destination of a redirected socket in a Linux
    // sockaddr_in / sockaddr_in6. The kernel supplies the address and length.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&storage) as libc::socklen_t;
    let level = if socket.local_addr()?.is_ipv4() {
        libc::SOL_IP
    } else {
        libc::SOL_IPV6
    };
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            level,
            ORIGINAL_DST,
            (&mut storage as *mut libc::sockaddr_storage).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    match i32::from(storage.ss_family) {
        libc::AF_INET if len as usize >= std::mem::size_of::<libc::sockaddr_in>() => {
            let addr =
                unsafe { *(&storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            Ok(SocketAddr::new(
                Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()).into(),
                u16::from_be(addr.sin_port),
            ))
        }
        libc::AF_INET6 if len as usize >= std::mem::size_of::<libc::sockaddr_in6>() => {
            let addr = unsafe {
                *(&storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>()
            };
            Ok(SocketAddr::new(
                Ipv6Addr::from(addr.sin6_addr.s6_addr).into(),
                u16::from_be(addr.sin6_port),
            ))
        }
        _ => Err(invalid()),
    }
}

pub async fn serve(gate: Arc<Gate>) -> io::Result<()> {
    let list = fs::read_to_string(DOMAINS)?;
    let domains: Vec<_> = list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect();
    if domains.is_empty() {
        return Err(invalid());
    }
    let proxy = Arc::new(Proxy { gate, domains });
    let mut listeners = Vec::new();
    for (address, mode) in [
        ("127.0.0.1:18081", Inbound::Http { strict: false }),
        ("127.0.0.1:18082", Inbound::Http { strict: true }),
        ("127.0.0.1:11081", Inbound::Socks),
        ("127.0.0.1:15443", Inbound::Direct),
        ("[::1]:15443", Inbound::Direct),
    ] {
        listeners.push((TcpListener::bind(address).await?, mode));
    }
    signal_ready()?;
    let slots = Arc::new(tokio::sync::Semaphore::new(512));
    let mut servers = tokio::task::JoinSet::new();
    for (listener, mode) in listeners {
        let proxy = Arc::clone(&proxy);
        let slots = Arc::clone(&slots);
        servers.spawn(async move {
            loop {
                let (stream, _) = listener.accept().await?;
                let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
                    continue;
                };
                let proxy = Arc::clone(&proxy);
                tokio::spawn(async move {
                    let _slot = slot;
                    match mode {
                        Inbound::Socks => {
                            let _ = proxy.serve_socks(stream).await;
                        }
                        Inbound::Http { strict } => proxy.serve_http(stream, strict, None).await,
                        Inbound::Direct => {
                            let _ = proxy.redirected(stream).await;
                        }
                    }
                });
            }
        });
    }
    servers
        .join_next()
        .await
        .ok_or_else(invalid)?
        .map_err(io::Error::other)?
}

fn signal_ready() -> io::Result<()> {
    let Some(socket) = env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let addr = match socket.as_encoded_bytes().strip_prefix(b"@") {
        Some(abstract_name) => LocalAddr::from_abstract_name(abstract_name)?,
        None => LocalAddr::from_pathname(socket)?,
    };
    UnixDatagram::unbound()?.send_to_addr(b"READY=1", &addr)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recognizes_web_prefaces_and_preserves_binary_bytes() {
        for (bytes, protocol) in [
            (&b"GET / HTTP/1.1\r\n"[..], Protocol::Http),
            (&b"MKCOL /folder HTTP/1.1\r\n"[..], Protocol::Http),
            (&b"\r\nGET / HTTP/1.1\r\n"[..], Protocol::Http),
            (&b"\x16\x03\x01\0\x20"[..], Protocol::Tls),
            (&b"\x16\x01\x02\x03\x04"[..], Protocol::Other),
            (&b"\x01\x02\xff"[..], Protocol::Other),
        ] {
            let mut stream = Cursor::new(bytes);
            let mut prefix = Vec::new();
            assert_eq!(
                connection_protocol(&mut stream, &mut prefix).await.unwrap(),
                protocol
            );
            stream.read_to_end(&mut prefix).await.unwrap();
            assert_eq!(prefix, bytes);
        }
    }
}
