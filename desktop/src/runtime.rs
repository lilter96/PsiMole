use crate::platform::{EngineJob, SystemProxy, attach_job, read_json, write_json};
use eframe::egui;
use psimole::guard::{self, Gate, Revision};
use psimole::speed::{self, Route, Speed};
use psimole::tunnel::{Notice, REGIONS, Service, Settings, Tunnel, db_ip_country, parse_trace};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::num::NonZeroU16;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

const ENGINE_URL: &str = "https://raw.githubusercontent.com/Psiphon-Labs/psiphon-tunnel-core-binaries/33ff79e89260f3adf92e846b96b3369b86a819d8/windows/psiphon-tunnel-core-i686.exe";
const ENGINE_SHA: &str = "34139a07deca9e9b94e5b55ebdf5e2583ca87f3d838873f105488681d22f4a0e";
const CONFIG_URL: &str = "https://raw.githubusercontent.com/SpherionOS/PsiphonLinux/899eb8e3f3020473a184401ad67f80326fb999df/psiphon.config";
const CONFIG_SHA: &str = "00c240e142484d252f07ad1f4d1f6ad56b2ac092e1344b51e0305f7517725c09";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Off,
    Preparing,
    Connecting,
    Connected,
    Reconnecting,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    Disabled,
    Blocked,
    Verifying,
    Allowed,
    PortUnavailable,
}

#[derive(Clone, Copy, Default)]
pub enum SpeedTest {
    #[default]
    Idle,
    Running,
    Measured(Speed),
    Failed,
}

#[derive(Clone)]
pub struct Snapshot {
    pub settings: Settings,
    pub phase: Phase,
    pub tunnel: Tunnel,
    pub protection: [Protection; 2],
    pub regions: Vec<String>,
    pub error: Option<String>,
    pub speed: SpeedTest,
}

impl Snapshot {
    fn new(settings: Settings) -> Self {
        Self {
            settings,
            phase: Phase::Off,
            tunnel: Tunnel::default(),
            protection: [Protection::Disabled; 2],
            regions: REGIONS.iter().map(|region| (*region).to_owned()).collect(),
            error: None,
            speed: SpeedTest::Idle,
        }
    }
}

pub enum Action {
    Connect,
    Disconnect,
    Region(String),
    Lock(Service, Option<String>),
    SystemProxy(bool),
    Language(bool),
    Measure,
    Quit,
}

enum Event {
    Action(Action),
    Prepared(u64, io::Result<Engine>),
    Notice(u64, Revision, Notice),
    EngineClosed(u64, String),
    Proof(Proof, bool),
    Measured(u64, Revision, io::Result<Speed>),
    GuardClosed(Service),
}

#[derive(Clone)]
struct Proof {
    run: u64,
    revision: Revision,
    country: String,
    socks: u16,
}

pub struct ClientHandle {
    events: mpsc::Sender<Event>,
    snapshots: mpsc::Receiver<Snapshot>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ClientHandle {
    pub fn start(dir: PathBuf, context: egui::Context) -> io::Result<(Self, Snapshot)> {
        let settings: Settings = read_json(&dir.join("settings.json"))?.unwrap_or_default();
        if !settings.valid() {
            return Err(io::Error::other("Invalid country code in PsiMole settings"));
        }
        let proxy = SystemProxy::new(dir.clone())?;
        let gate = Arc::new(Gate::default());
        gate.configure(settings.countries.clone());
        let (events, inbox) = mpsc::channel();
        let (publish, snapshots) = mpsc::channel();
        let initial = Snapshot::new(settings);
        let mut runtime = Runtime {
            dir,
            gate,
            proxy,
            net: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
            guards: [false; 2],
            status: initial.clone(),
            events: events.clone(),
            run: None,
            revision: None,
            proof: None,
            job: None,
        };
        let worker = thread::Builder::new()
            .name("psimole-supervisor".into())
            .spawn(move || {
                runtime.start_guards();
                runtime.update_protection();
                let _ = publish.send(runtime.status.clone());
                context.request_repaint();
                while let Ok(event) = inbox.recv() {
                    if !runtime.handle(event) {
                        break;
                    }
                    runtime.update_protection();
                    let _ = publish.send(runtime.status.clone());
                    context.request_repaint();
                }
            })?;
        Ok((
            Self {
                events,
                snapshots,
                worker: Some(worker),
            },
            initial,
        ))
    }

    pub fn send(&self, action: Action) {
        let _ = self.events.send(Event::Action(action));
    }

    pub fn refresh(&self, snapshot: &mut Snapshot) {
        if let Some(latest) = self.snapshots.try_iter().last() {
            *snapshot = latest;
        }
    }

    pub fn shutdown(&mut self) {
        self.send(Action::Quit);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Runtime {
    dir: PathBuf,
    gate: Arc<Gate>,
    proxy: SystemProxy,
    net: tokio::runtime::Runtime,
    guards: [bool; 2],
    status: Snapshot,
    events: mpsc::Sender<Event>,
    run: Option<u64>,
    revision: Option<Revision>,
    proof: Option<(Revision, Option<bool>)>,
    job: Option<EngineJob>,
}

impl Runtime {
    fn start_guards(&mut self) {
        for (index, service) in Service::ALL.into_iter().enumerate() {
            let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, service.port())) else {
                continue;
            };
            if listener.set_nonblocking(true).is_err() {
                continue;
            }
            let gate = Arc::clone(&self.gate);
            let events = self.events.clone();
            self.net.spawn(async move {
                if let Ok(listener) = tokio::net::TcpListener::from_std(listener) {
                    let _ = guard::serve(listener, gate, service).await;
                }
                let _ = events.send(Event::GuardClosed(service));
            });
            self.guards[index] = true;
        }
    }

    fn connect(&mut self) {
        self.stop();
        if self.status.error.is_some() {
            self.status.phase = Phase::Failed;
            return;
        }
        let run = self.gate.begin();
        self.run = Some(run);
        self.revision = self.gate.revision(run);
        self.status.phase = Phase::Preparing;
        let dir = self.dir.join("psiphon");
        let events = self.events.clone();
        thread::spawn(move || {
            let _ = events.send(Event::Prepared(run, Engine::prepare(&dir)));
        });
    }

    fn stop(&mut self) {
        if let Some(run) = self.run.take() {
            self.gate.revoke(run);
        }
        self.revision = None;
        self.proof = None;
        // Restore the system setting before closing the proxy it may point to.
        if let Err(error) = self.proxy.restore() {
            self.status.error = Some(error.to_string());
        }
        if let Some(job) = self.job.take()
            && let Err(error) = job.stop()
        {
            self.status.error = Some(error.to_string());
        }
        self.status.tunnel = Tunnel::default();
        self.status.phase = Phase::Off;
        self.status.speed = SpeedTest::Idle;
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.stop();
        self.status.phase = Phase::Failed;
        self.status.error = Some(match self.status.error.take() {
            Some(error) => format!("{}\n{error}", message.into()),
            None => message.into(),
        });
    }

    fn synchronize_proxy(&mut self) {
        let result = match (
            self.status.settings.system_proxy,
            self.status.phase,
            self.status.tunnel.http,
        ) {
            (true, Phase::Connected, Some(port)) => self.proxy.apply(port),
            _ => self.proxy.restore(),
        };
        if let Err(error) = result {
            self.status.error = Some(error.to_string());
        }
    }

    fn settings_changed(&mut self, action: Action) {
        let mut settings = self.status.settings.clone();
        let reconnect = match action {
            Action::Region(country) => {
                if settings.region == country {
                    return;
                }
                settings.region = country;
                self.run.is_some()
            }
            Action::Lock(service, country) => {
                settings.countries.set(service, country);
                false
            }
            Action::SystemProxy(enabled) => {
                settings.system_proxy = enabled;
                false
            }
            Action::Language(russian) => {
                settings.russian = Some(russian);
                false
            }
            _ => unreachable!(),
        };
        if !settings.valid() {
            return;
        }
        // Changing or removing a lock closes that service's active streams immediately.
        if reconnect && let Some(run) = self.run {
            self.gate.revoke(run);
        }
        self.gate.configure(settings.countries.clone());
        self.status.settings = settings;
        if let Err(error) = write_json(&self.dir.join("settings.json"), &self.status.settings) {
            self.status.error = Some(error.to_string());
        }
        if reconnect {
            let settings_error = self.status.error.take();
            self.connect();
            if self.status.error.is_none() {
                self.status.error = settings_error;
            }
        } else {
            self.synchronize_proxy();
        }
    }

    fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Action(Action::Quit) => {
                self.stop();
                return false;
            }
            Event::Action(Action::Connect) if self.run.is_none() => {
                self.status.error = None;
                self.connect();
            }
            Event::Action(Action::Connect) => {}
            Event::Action(Action::Disconnect) => {
                self.status.error = None;
                self.stop();
            }
            Event::Action(Action::Measure) => self.measure(),
            Event::Action(action) => {
                self.status.error = None;
                self.settings_changed(action);
            }
            Event::Prepared(run, result) if self.run == Some(run) => {
                let job = result.and_then(|engine| {
                    engine.spawn(
                        run,
                        &self.status.settings.region,
                        Arc::clone(&self.gate),
                        self.events.clone(),
                    )
                });
                match job {
                    Ok(job) => {
                        self.job = Some(job);
                        self.status.phase = Phase::Connecting;
                    }
                    Err(error) => self.fail(error.to_string()),
                }
            }
            Event::Notice(run, revision, notice) if self.run == Some(run) => {
                self.notice(revision, notice);
            }
            Event::EngineClosed(run, reason) if self.run == Some(run) => self.fail(reason),
            Event::Proof(proof, allowed) => {
                if self.run == Some(proof.run)
                    && self.revision == Some(proof.revision)
                    && self.gate.revision(proof.run) == Some(proof.revision)
                {
                    self.proof = Some((proof.revision, Some(allowed)));
                    if allowed {
                        self.gate
                            .approve(proof.run, proof.revision, proof.country, proof.socks);
                    }
                }
            }
            Event::Measured(run, revision, result) => {
                if self.run == Some(run) && self.gate.revision(run) == Some(revision) {
                    self.status.speed = match result {
                        Ok(measurement) => SpeedTest::Measured(measurement),
                        Err(_) => SpeedTest::Failed,
                    };
                }
            }
            Event::GuardClosed(service) => {
                self.guards[usize::from(service == Service::ChatGpt)] = false;
            }
            _ => {}
        }
        self.maybe_verify();
        true
    }

    fn notice(&mut self, revision: Revision, notice: Notice) {
        if let Notice::Regions(ref regions) = notice {
            self.status.regions = regions.clone();
            self.status.regions.sort();
            self.status.regions.dedup();
        }
        if notice == Notice::PortInUse {
            self.fail("One of Psiphon's local proxy ports is already in use (8081 or 1081).");
            return;
        }
        if notice == Notice::Exiting {
            self.fail("Psiphon closed the tunnel.");
            return;
        }
        if self.revision != Some(revision) {
            self.status.speed = SpeedTest::Idle;
        }
        self.revision = Some(revision);
        self.status.tunnel.update(&notice, Instant::now());
        if matches!(
            notice,
            Notice::Tunnels(_) | Notice::Region(_) | Notice::Http(_)
        ) {
            self.status.phase = if self.status.tunnel.connected_at.is_some()
                && self.status.tunnel.region.is_some()
            {
                Phase::Connected
            } else if self.status.tunnel.connected_once {
                Phase::Reconnecting
            } else {
                Phase::Connecting
            };
            self.synchronize_proxy();
        }
    }

    fn measure(&mut self) {
        let (Some(run), Some(revision), Some(port)) = (
            self.run,
            self.revision,
            self.status.tunnel.http.and_then(NonZeroU16::new),
        ) else {
            return;
        };
        if self.status.phase != Phase::Connected
            || matches!(self.status.speed, SpeedTest::Running)
            || self.gate.revision(run) != Some(revision)
        {
            return;
        }
        self.status.speed = SpeedTest::Running;
        let events = self.events.clone();
        let gate = Arc::clone(&self.gate);
        thread::spawn(move || {
            let result =
                speed::measure(Route::Proxy(port), || gate.revision(run) == Some(revision));
            let _ = events.send(Event::Measured(run, revision, result));
        });
    }

    fn maybe_verify(&mut self) {
        let (Some(run), Some(revision)) = (self.run, self.revision) else {
            return;
        };
        if self.gate.revision(run) != Some(revision)
            || self.gate.verified(revision)
            || self.proof.is_some_and(|(started, _)| started == revision)
        {
            return;
        }
        let country = self.status.settings.region.as_str();
        let Some(socks) = Service::ALL.into_iter().find_map(|service| {
            let allowed = self.status.settings.countries.get(service)?;
            self.status.tunnel.guard_port(country, allowed)
        }) else {
            return;
        };
        let proof = Proof {
            run,
            revision,
            country: country.to_owned(),
            socks,
        };
        let events = self.events.clone();
        self.proof = Some((revision, None));
        let gate = Arc::clone(&self.gate);
        thread::spawn(move || {
            let allowed = verify_country(&proof, &gate);
            let _ = events.send(Event::Proof(proof, allowed));
        });
    }

    fn update_protection(&mut self) {
        for (index, service) in Service::ALL.into_iter().enumerate() {
            self.status.protection[index] = if self.status.settings.countries.get(service).is_none()
            {
                Protection::Disabled
            } else if !self.guards[index] {
                Protection::PortUnavailable
            } else if self.gate.ready(service) {
                Protection::Allowed
            } else if self.status.settings.countries.get(service)
                == Some(&self.status.settings.region)
                && self
                    .revision
                    .is_some_and(|revision| self.proof == Some((revision, None)))
            {
                Protection::Verifying
            } else {
                Protection::Blocked
            };
        }
    }
}

fn fresh_client() -> reqwest::blocking::ClientBuilder {
    Client::builder()
        .no_proxy()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
}

fn verify_country(proof: &Proof, gate: &Gate) -> bool {
    let check = |url: &str| -> Option<String> {
        if gate.revision(proof.run) != Some(proof.revision) {
            return None;
        }
        let proxy = reqwest::Proxy::all(format!("socks5h://127.0.0.1:{}", proof.socks)).ok()?;
        let client = fresh_client()
            .proxy(proxy)
            .timeout(Duration::from_secs(8))
            .build()
            .ok()?;
        let response = client.get(url).send().ok()?.error_for_status().ok()?;
        let mut text = String::new();
        response.take(64 * 1024).read_to_string(&mut text).ok()?;
        Some(text)
    };
    let Some(ip) = check("https://www.cloudflare.com/cdn-cgi/trace")
        .and_then(|trace| parse_trace(&trace, &proof.country))
    else {
        return false;
    };
    if let Some(country) = check(&format!("https://ipinfo.io/{ip}/country")) {
        return country.trim() == proof.country;
    }
    check(&format!("https://api.db-ip.com/v2/free/{ip}"))
        .is_some_and(|body| db_ip_country(&body, ip, &proof.country))
}

struct Engine {
    dir: PathBuf,
    executable: PathBuf,
    config: PathBuf,
}

impl Engine {
    fn prepare(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let executable = dir.join("psiphon-core-33ff79e.exe");
        let config = dir.join("base-899eb8e.json");
        let client = fresh_client()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(io::Error::other)?;
        download(
            &client,
            ENGINE_URL,
            &executable,
            ENGINE_SHA,
            64 * 1024 * 1024,
        )?;
        download(&client, CONFIG_URL, &config, CONFIG_SHA, 64 * 1024)?;
        Ok(Self {
            dir: dir.to_owned(),
            executable,
            config,
        })
    }

    fn spawn(
        self,
        run: u64,
        region: &str,
        gate: Arc<Gate>,
        events: mpsc::Sender<Event>,
    ) -> io::Result<EngineJob> {
        let mut config: Value = read_json(&self.config)?
            .ok_or_else(|| io::Error::other("Missing Psiphon configuration"))?;
        let data = self.dir.join("data");
        fs::create_dir_all(&data)?;
        config["EgressRegion"] = json!(region);
        config["LocalHttpProxyPort"] = json!(8081);
        config["LocalSocksProxyPort"] = json!(1081);
        config["TunnelPoolSize"] = json!(1);
        // Allow up to 2 MiB of in-flight data per SSH channel on high-delay routes.
        config["SSHChannelWindowSize"] = json!(64);
        config["EmitBytesTransferred"] = json!(true);
        config["SplitTunnelOwnRegion"] = json!(false);
        config["SplitTunnelRegions"] = json!([]);
        config["EnableLightProxyFallback"] = json!(false);
        // An empty interface binds the upstream listener to loopback.
        config["ListenInterface"] = json!("");
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(file.as_file_mut(), &config)?;
        file.as_file_mut().flush()?;
        let config = file.into_temp_path();

        let mut command = Command::new(&self.executable);
        command
            .arg("-config")
            .arg(&config)
            .arg("-dataRootDirectory")
            .arg(&data)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"] {
            command.env_remove(key);
        }
        let mut child = command.spawn()?;
        let job = match attach_job(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let mut output = BufReader::new(child.stderr.take().expect("stderr is piped"));
        let notices = events.clone();
        let reader_gate = Arc::clone(&gate);
        thread::spawn(move || {
            let mut line = Vec::new();
            while let Ok(size) = output.read_until(b'\n', &mut line) {
                if size == 0 {
                    break;
                }
                if let Some(notice) = Notice::parse(&line) {
                    let revision = if notice.changes_route() {
                        reader_gate.revoke(run)
                    } else {
                        reader_gate.revision(run)
                    };
                    let Some(revision) = revision else {
                        break;
                    };
                    if notices.send(Event::Notice(run, revision, notice)).is_err() {
                        break;
                    }
                }
                line.clear();
            }
            if reader_gate.revoke(run).is_some() {
                let _ = notices.send(Event::EngineClosed(
                    run,
                    "Psiphon stopped reporting tunnel state.".into(),
                ));
            }
        });
        thread::spawn(move || {
            let result = child.wait();
            gate.revoke(run);
            drop(config);
            let reason = match result {
                Ok(status) => format!("Psiphon exited ({status})."),
                Err(error) => error.to_string(),
            };
            let _ = events.send(Event::EngineClosed(run, reason));
        });
        Ok(job)
    }
}

fn checksum(mut input: impl Read) -> io::Result<String> {
    let mut digest = Sha256::new();
    io::copy(&mut input, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn download(client: &Client, url: &str, path: &Path, expected: &str, limit: u64) -> io::Result<()> {
    if let Ok(file) = File::open(path)
        && checksum(file.take(limit + 1))? == expected
    {
        return Ok(());
    }
    let response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(io::Error::other)?;
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    let size = io::copy(&mut response.take(limit + 1), &mut temporary)?;
    if size > limit || checksum(File::open(temporary.path())?)? != expected {
        return Err(io::Error::other(
            "The downloaded Psiphon file failed its SHA-256 check.",
        ));
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_download_digest() {
        assert_eq!(
            checksum(&b"abc"[..]).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    #[ignore = "requires a real Windows host with Internet access and free local Psiphon ports"]
    fn live_austrian_tunnel() {
        let data = tempfile::tempdir().unwrap();
        let engine = Engine::prepare(data.path()).unwrap();
        let (events, inbox) = mpsc::channel();
        let gate = Arc::new(Gate::default());
        let run = gate.begin();
        let job = engine.spawn(run, "AT", Arc::clone(&gate), events).unwrap();
        let mut tunnel = Tunnel::default();
        let started = Instant::now();
        let limit = started + Duration::from_secs(120);
        let (proof, http) = loop {
            let event = inbox
                .recv_timeout(limit.saturating_duration_since(Instant::now()))
                .expect("Psiphon did not establish an Austrian tunnel");
            match event {
                Event::Notice(_, revision, notice) => {
                    assert!(
                        !matches!(notice, Notice::PortInUse | Notice::Exiting),
                        "{notice:?}"
                    );
                    tunnel.update(&notice, Instant::now());
                    if let (Some(socks), Some(http)) = (tunnel.guard_port("AT", "AT"), tunnel.http)
                    {
                        break (
                            Proof {
                                run,
                                revision,
                                country: "AT".into(),
                                socks,
                            },
                            http,
                        );
                    }
                }
                Event::EngineClosed(_, error) => panic!("{error}"),
                _ => {}
            }
        };
        assert!(
            verify_country(&proof, &gate),
            "The exit did not pass both geographic checks"
        );
        let route = Route::Proxy(NonZeroU16::new(http).unwrap());
        let data = speed::measure(route, || gate.revision(run) == Some(proof.revision)).unwrap();
        assert_eq!(gate.revision(run), Some(proof.revision));
        println!(
            "Austrian tunnel: {:.2} Mbps down, {:.2} Mbps up",
            data.down_mbps, data.up_mbps
        );
        job.stop().unwrap();
    }
}
