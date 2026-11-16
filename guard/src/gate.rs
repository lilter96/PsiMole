use crate::watch::{Changes, POLICY_FILE};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const GEO_CACHE_LIFETIME: Duration = Duration::from_secs(600);

fn country_code(country: &str) -> bool {
    country.len() == 2 && country.bytes().all(|byte| byte.is_ascii_uppercase())
}

pub fn policy_country(path: &Path) -> Option<String> {
    let policy: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let country = policy["country"].as_str()?;
    (policy["service"] == "claude" && country_code(country)).then(|| country.to_owned())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Connection {
    pid: u32,
    start: String,
    log_inode: u64,
    notice_offset: u64,
    country: String,
    config: Value,
}

#[derive(Debug, Eq, PartialEq)]
struct Report {
    country: String,
    offset: u64,
}

fn connected_region(log: impl BufRead, skip: u64) -> Option<Report> {
    let mut offset = skip;
    let (mut count, mut report) = (0, None);
    for (index, line) in log.split(b'\n').enumerate() {
        let line = line.ok()?;
        offset += line.len() as u64 + 1;
        if line.is_empty() || (skip > 0 && index == 0) {
            continue;
        }
        let notice: Value = serde_json::from_slice(&line).ok()?;
        match notice["noticeType"].as_str() {
            Some("Tunnels") => {
                count = notice["data"]["count"].as_u64().unwrap_or(0);
                // Even a reconnect to the same country must supply a new
                // ConnectedServerRegion notice after the Tunnels notice.
                report = None;
            }
            Some("ConnectedServerRegion") if count == 1 => {
                report = Some(Report {
                    country: notice["data"]["serverRegion"].as_str()?.to_owned(),
                    offset,
                });
            }
            Some("Exiting") => {
                count = 0;
                report = None;
            }
            _ => {}
        }
    }
    report
}

fn snapshot(dir: &Path, socks: u16, http: u16) -> Option<Connection> {
    let country = policy_country(&dir.join(POLICY_FILE))?;
    let conf = dir.join("client.conf");
    let config: Value = serde_json::from_slice(&fs::read(&conf).ok()?).ok()?;
    if config["EgressRegion"] != country
        || config["LocalSocksProxyPort"] != socks
        || config["LocalHttpProxyPort"] != http
        || config["TunnelPoolSize"].as_u64().unwrap_or(1) > 1
        || config["SplitTunnelOwnRegion"].as_bool().unwrap_or(false)
        || config["SplitTunnelRegions"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
        || config["EnableLightProxyFallback"]
            .as_bool()
            .unwrap_or(false)
    {
        return None;
    }

    let pid = fs::read_to_string(dir.join("psiphon.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let args = fs::read(proc.join("cmdline")).ok()?;
    let mut args = args.split(|byte| *byte == 0);
    if args.next()? != dir.join("psiphon").as_os_str().as_encoded_bytes()
        || args.next()? != b"-config"
        || args.next()? != conf.as_os_str().as_encoded_bytes()
    {
        return None;
    }
    let stat = fs::read_to_string(proc.join("stat")).ok()?;
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    if !matches!(fields.next()?, "R" | "S" | "D" | "I") {
        return None;
    }
    let start = fields.nth(18)?.to_owned();
    let mut log = File::open(dir.join("psiphon.log")).ok()?;
    let meta = log.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let skip = meta.len().saturating_sub(512 * 1024);
    log.seek(SeekFrom::Start(skip)).ok()?;
    let report = connected_region(BufReader::new(log.take(512 * 1024)), skip)?;
    if report.country != country {
        return None;
    }
    Some(Connection {
        pid,
        start,
        log_inode: meta.ino(),
        notice_offset: report.offset,
        country,
        config,
    })
}

fn trace(url: &str, proxy: SocketAddr) -> Option<String> {
    let result = Command::new("/usr/bin/curl")
        .env_clear()
        .args([
            "--disable",
            "--socks5-hostname",
            &proxy.to_string(),
            "--noproxy",
            "",
            "--silent",
            "--fail",
            "--max-time",
            "5",
            "--max-filesize",
            "8192",
            url,
        ])
        .output()
        .ok()?;
    result
        .status
        .success()
        .then(|| String::from_utf8(result.stdout).ok())?
}

fn db_ip_country(report: &str, ip: IpAddr) -> Option<String> {
    let report: Value = serde_json::from_str(report).ok()?;
    let reported_ip: IpAddr = report["ipAddress"].as_str()?.parse().ok()?;
    let country = report["countryCode"].as_str()?;
    (reported_ip == ip && country_code(country)).then(|| country.to_owned())
}

#[derive(Clone)]
pub struct Approval {
    revision: u64,
    pub country: String,
}

#[derive(Default)]
struct State {
    current: Option<Connection>,
    revision: u64,
    verified: bool,
    failed: bool,
    watching: bool,
    active: HashMap<u64, TcpStream>,
    next_id: u64,
}

impl State {
    fn revoke(&mut self) {
        self.revision += 1;
        if self.verified {
            eprintln!("Psiphon state changed; Claude access blocked");
        }
        self.verified = false;
        self.failed = false;
        for stream in self.active.drain().map(|(_, stream)| stream) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn phase(&self) -> &'static str {
        match (
            self.watching,
            self.current.is_some(),
            self.verified,
            self.failed,
        ) {
            (false, _, _, _) => "unavailable",
            (_, false, _, _) => "blocked",
            (_, _, true, _) => "ready",
            (_, _, _, true) => "failed",
            _ => "checking",
        }
    }
}

pub struct Gate {
    dir: PathBuf,
    status_file: PathBuf,
    http: u16,
    pub socks: SocketAddr,
    state: Mutex<State>,
    schedule: SyncSender<()>,
}

impl Gate {
    fn publish(&self, state: &State) {
        let tmp = self.status_file.with_extension("tmp");
        let value = json!({
            "service": "claude",
            "country": policy_country(&self.dir.join(POLICY_FILE)),
            "phase": state.phase(),
            "pid": std::process::id(),
        });
        // This is only a UI readout. The launch and network paths always
        // acquire permission from the live state under the mutex.
        if fs::write(&tmp, value.to_string()).is_ok() {
            let _ = fs::rename(tmp, &self.status_file);
        }
    }

    fn reconcile(&self, state: &mut State, reset: bool) {
        if !state.watching {
            return;
        }
        let current = snapshot(&self.dir, self.socks.port(), self.http);
        if reset || current != state.current {
            state.revoke();
            state.current = current;
            self.publish(state);
            if state.current.is_some() {
                let _ = self.schedule.try_send(());
            }
        }
    }

    pub fn ready(&self) -> Option<Approval> {
        let mut state = self.state.lock().unwrap();
        self.reconcile(&mut state, false);
        if !state.verified || !state.watching {
            return None;
        }
        Some(Approval {
            revision: state.revision,
            country: state.current.as_ref()?.country.clone(),
        })
    }

    fn monitor(&self, mut events: Changes) {
        let result = (|| -> io::Result<()> {
            let mut reset = true;
            loop {
                let pid = {
                    let mut state = self.state.lock().unwrap();
                    self.reconcile(&mut state, reset);
                    state.current.as_ref().map(|connection| connection.pid)
                };
                reset = events.wait(pid)?;
            }
        })();
        let mut state = self.state.lock().unwrap();
        state.watching = false;
        state.current = None;
        state.revoke();
        self.publish(&state);
        if let Err(err) = result {
            eprintln!("Psiphon events unavailable: {err}");
        }
    }

    fn verifying(&self, revision: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        self.reconcile(&mut state, false);
        state.watching && state.revision == revision
    }

    fn independent_country(&self, revision: u64, ip: IpAddr) -> Option<String> {
        if let Some(country) = trace(&format!("https://ipinfo.io/{ip}/country"), self.socks) {
            return Some(country.trim().to_owned());
        }
        // A transport failure has no country to compare. Keep a successful
        // primary answer authoritative, including when it disagrees.
        if !self.verifying(revision) {
            return None;
        }
        let report = trace(&format!("https://api.db-ip.com/v2/free/{ip}"), self.socks)?;
        db_ip_country(&report, ip)
    }

    fn verify_exit(
        &self,
        revision: u64,
        connection: &Connection,
        cache: &mut Option<(IpAddr, String, Instant)>,
    ) -> Option<IpAddr> {
        if !self.verifying(revision) {
            return None;
        }
        let report = trace("https://www.cloudflare.com/cdn-cgi/trace", self.socks)?;
        let reported_country = report.lines().find_map(|line| line.strip_prefix("loc="))?;
        if reported_country != connection.country || !self.verifying(revision) {
            return None;
        }
        let ip: IpAddr = report
            .lines()
            .find_map(|line| line.strip_prefix("ip="))?
            .parse()
            .ok()?;
        if !cache.as_ref().is_some_and(|(known_ip, country, checked)| {
            *known_ip == ip
                && *country == connection.country
                && checked.elapsed() < GEO_CACHE_LIFETIME
        }) {
            if self.independent_country(revision, ip)? != connection.country {
                return None;
            }
            *cache = Some((ip, connection.country.clone(), Instant::now()));
        }
        Some(ip)
    }

    fn verify(&self, work: Receiver<()>) {
        let mut cache = None;
        let mut tried = None;
        // Wake only for a new connection generation. There is no network
        // timer. A failed proof stays closed until another relevant event.
        for () in work {
            let (connection, revision) = {
                let mut state = self.state.lock().unwrap();
                self.reconcile(&mut state, false);
                let Some(connection) = state.current.clone() else {
                    continue;
                };
                (connection, state.revision)
            };
            if tried == Some(revision) {
                continue;
            }
            tried = Some(revision);
            let good = self
                .verify_exit(revision, &connection, &mut cache)
                .is_some();
            let mut state = self.state.lock().unwrap();
            self.reconcile(&mut state, false);
            if state.revision != revision || state.current.as_ref() != Some(&connection) {
                continue;
            }
            state.verified = good;
            state.failed = !good;
            self.publish(&state);
            if good {
                eprintln!("Psiphon exit confirmed for {}", connection.country);
            } else {
                eprintln!(
                    "Psiphon exit could not be verified for {}",
                    connection.country
                );
            }
        }
    }

    pub fn track(self: &Arc<Self>, upstream: &TcpStream, before: &Approval) -> io::Result<Permit> {
        // Opening the local socket sent no destination to Psiphon. Reconcile
        // once more before any SOCKS request goes into that socket.
        let mut state = self.state.lock().unwrap();
        self.reconcile(&mut state, false);
        if !state.verified || !state.watching || state.revision != before.revision {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let id = state.next_id;
        state.next_id += 1;
        state.active.insert(id, upstream.try_clone()?);
        Ok(Permit {
            gate: Arc::clone(self),
            id,
        })
    }
}

pub struct Permit {
    gate: Arc<Gate>,
    id: u64,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.gate.state.lock().unwrap().active.remove(&self.id);
    }
}

pub fn start(dir: &Path, path: &str, socks: u16, http: u16) -> io::Result<Arc<Gate>> {
    let events = Changes::new(dir)?;
    let _ = fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let (schedule, work) = mpsc::sync_channel(1);
    let gate = Arc::new(Gate {
        dir: dir.to_owned(),
        status_file: Path::new(path).with_extension("json"),
        http,
        socks: SocketAddr::from(([127, 0, 0, 1], socks)),
        state: Mutex::new(State {
            watching: true,
            ..State::default()
        }),
        schedule,
    });
    gate.publish(&gate.state.lock().unwrap());
    {
        let gate = Arc::clone(&gate);
        thread::Builder::new()
            .name("psiphon-events".into())
            .spawn(move || gate.monitor(events))?;
    }
    {
        let gate = Arc::clone(&gate);
        thread::Builder::new()
            .name("exit-verifier".into())
            .spawn(move || gate.verify(work))?;
    }
    {
        let gate = Arc::clone(&gate);
        thread::Builder::new()
            .name("guard-status".into())
            .spawn(move || {
                for mut stream in listener.incoming().map_while(Result::ok) {
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    if let Some(permit) = gate.ready() {
                        let _ = writeln!(&mut stream, "{}", permit.country);
                    }
                }
            })?;
    }
    Ok(gate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const UP: &str = r#"{"noticeType":"Tunnels","data":{"count":1}}"#;
    const DOWN: &str = r#"{"noticeType":"Tunnels","data":{"count":0}}"#;
    const AT: &str = r#"{"noticeType":"ConnectedServerRegion","data":{"serverRegion":"AT"}}"#;
    const DE: &str = r#"{"noticeType":"ConnectedServerRegion","data":{"serverRegion":"DE"}}"#;

    fn notices(lines: &[&str]) -> Option<Report> {
        connected_region(lines.join("\n").as_bytes(), 0)
    }

    #[test]
    fn reconnect_waits_for_a_new_country_notice() {
        let first = notices(&[UP, AT]).unwrap();
        assert_eq!(first.country, "AT");
        assert!(notices(&[UP, AT, DOWN]).is_none());
        assert!(notices(&[UP, AT, DOWN, UP]).is_none());
        let second = notices(&[UP, AT, DOWN, UP, AT]).unwrap();
        assert_eq!(second.country, "AT");
        assert_ne!(first.offset, second.offset);
        assert_eq!(notices(&[UP, AT, DOWN, UP, DE]).unwrap().country, "DE");
    }

    #[test]
    fn no_notice_from_an_unconnected_or_multitunnel_state_is_usable() {
        let two = r#"{"noticeType":"Tunnels","data":{"count":2}}"#;
        let exiting = r#"{"noticeType":"Exiting","data":{}}"#;
        assert!(notices(&[AT]).is_none());
        assert!(notices(&[UP, AT, two, DE]).is_none());
        assert!(notices(&[UP, AT, exiting]).is_none());
        assert!(notices(&[UP, AT, r#"{"noticeType":"Tunnels"#]).is_none());
    }

    #[test]
    fn secondary_country_must_belong_to_the_same_ip() {
        let ip = "2001:db8::1".parse().unwrap();
        let report = r#"{"ipAddress":"2001:db8:0:0::1","countryCode":"AT"}"#;
        assert_eq!(db_ip_country(report, ip).as_deref(), Some("AT"));
        assert_eq!(db_ip_country(report, "2001:db8::2".parse().unwrap()), None);
        assert_eq!(
            db_ip_country(r#"{"ipAddress":"2001:db8::1","countryCode":"?"}"#, ip),
            None
        );
    }

    #[test]
    fn revoking_permission_closes_an_open_tcp_session() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (upstream, _) = listener.accept().unwrap();
        let mut state = State {
            verified: true,
            ..State::default()
        };
        state.active.insert(0, upstream.try_clone().unwrap());
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        state.revoke();
        assert_eq!(client.read(&mut [0u8; 8]).unwrap(), 0);
        assert!(!state.verified);
        assert!(state.active.is_empty());
        assert_eq!(state.revision, 1);
    }
}
