use crate::{raw_firewall, watch::POLICY_FILE, web};
use serde_json::{Value, json};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{self, Command};

const ETC: &str = "/etc/claude-at";
const BIN: &str = "/usr/local/bin/claude-at";
const MANIFEST: &str = "/etc/claude-at/install.json";
const FIREFOX: &str = "/etc/firefox/policies/policies.json";
const SYSTEMD: &str = "/etc/systemd/system";
const UNIT: &str = "claude-at.service";
const FIREWALL: &str = "claude-at-firewall.service";
const DNS: &str = "claude-at-dns.service";
const LINK: &str = "claude-at-dns-link.service";
const DOMAINS: &str = include_str!("../assets/domains.txt");

fn need_root() -> io::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        Ok(())
    } else {
        Err(io::Error::other("Run this command with sudo."))
    }
}

fn command(command: &str, args: &[&str]) -> io::Result<String> {
    let result = Command::new(command).args(args).output()?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(io::Error::other(format!(
            "{command} failed ({}): {}",
            result.status,
            stderr.trim()
        )));
    }
    String::from_utf8(result.stdout).map_err(io::Error::other)
}

fn json_file(path: &Path) -> io::Result<Value> {
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

fn write(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let tmp = path.with_extension(format!("new.{}", process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn country(input: &str) -> io::Result<String> {
    let country = input.to_ascii_uppercase();
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Err(io::Error::other(
            "Select a two-letter country code such as AT or DE.",
        ));
    }
    Ok(country)
}

fn home() -> io::Result<PathBuf> {
    env::home_dir().ok_or_else(|| io::Error::other("Could not find the account home directory."))
}

pub fn select(input: &str) -> io::Result<()> {
    let country = country(input)?;
    let value = json!({ "service": "claude", "country": country });
    write(
        &home()?.join("psiphon").join(POLICY_FILE),
        value.to_string().as_bytes(),
        0o600,
    )
}

fn proxy_port(config: &Value, field: &str) -> io::Result<u16> {
    let port = config[field]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .filter(|port| *port >= 1024 && ![11081, 15443, 18081, 18082].contains(port));
    port.ok_or_else(|| io::Error::other(format!("Invalid or reserved Psiphon port: {field}.")))
}

fn entries() -> impl Iterator<Item = &'static str> {
    DOMAINS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

fn dns_conf() -> String {
    let mut config = String::from(
        "port=10553\nlisten-address=169.254.227.53\nbind-interfaces\nno-resolv\nno-hosts\nno-poll\ncache-size=0\nlocal-ttl=0\n",
    );
    for entry in entries() {
        let domain = entry.trim_start_matches('=');
        config.push_str(&format!("local=/{domain}/\n"));
        if entry.starts_with('=') {
            config.push_str(&format!("host-record={domain},127.97.0.1\n"));
        } else {
            config.push_str(&format!("address=/{domain}/127.97.0.1\n"));
        }
    }
    config
}

fn account(user: &str) -> io::Result<(u32, String)> {
    if user.is_empty()
        || !user
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        return Err(io::Error::other("Invalid local account name."));
    }
    let result = command("/usr/bin/getent", &["passwd", user])?;
    let fields: Vec<_> = result.trim().split(':').collect();
    let uid = fields
        .get(2)
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|uid| *uid >= 1000)
        .ok_or_else(|| io::Error::other("Select a regular desktop user."))?;
    let home = fields
        .get(5)
        .copied()
        .filter(|home| {
            home.starts_with('/')
                && home
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'/' | b'.'))
        })
        .ok_or_else(|| {
            io::Error::other("The account needs an absolute, standard Linux home path.")
        })?;
    Ok((uid, home.to_owned()))
}

fn install_group() -> io::Result<u32> {
    if command("/usr/bin/getent", &["group", "claude-at"]).is_err() {
        command("/usr/sbin/groupadd", &["--system", "claude-at"])?;
    }
    command("/usr/bin/getent", &["group", "claude-at"])?
        .trim()
        .split(':')
        .nth(2)
        .and_then(|gid| gid.parse().ok())
        .ok_or_else(|| io::Error::other("Could not create the guard group."))
}

pub fn install(user: &str, region: &str, manage_firefox: bool) -> io::Result<()> {
    need_root()?;
    let region = country(region)?;
    let (uid, home) = account(user)?;
    let psi_dir = format!("{home}/psiphon");
    let config = json_file(&Path::new(&psi_dir).join("client.conf"))?;
    let socks = proxy_port(&config, "LocalSocksProxyPort")?;
    let http = proxy_port(&config, "LocalHttpProxyPort")?;
    if socks == http
        || config["ListenInterface"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    {
        return Err(io::Error::other(
            "Use distinct Psiphon proxy ports bound to its default loopback interface.",
        ));
    }
    if Path::new(MANIFEST).exists() && json_file(Path::new(MANIFEST))?["user"] != user {
        return Err(io::Error::other(
            "This installation already protects a different local user.",
        ));
    }
    command(
        "/usr/bin/systemctl",
        &["is-active", "systemd-resolved.service"],
    )?;
    if !fs::read_to_string("/etc/resolv.conf")?
        .lines()
        .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["nameserver", "127.0.0.53"])
    {
        return Err(io::Error::other(
            "This installer needs the systemd-resolved stub at 127.0.0.53.",
        ));
    }
    let previous_firefox = Path::new(MANIFEST).exists()
        && json_file(Path::new(MANIFEST))?["firefox"]
            .as_bool()
            .unwrap_or(false);
    let manage_firefox = manage_firefox || previous_firefox;
    let firefox = include_str!("../assets/firefox-policies.json");
    if manage_firefox && Path::new(FIREFOX).exists() {
        let current = json_file(Path::new(FIREFOX))?;
        let wanted = serde_json::from_str::<Value>(firefox).map_err(io::Error::other)?;
        let was_ours = previous_firefox
            && json_file(&Path::new(ETC).join("firefox-installed.json"))
                .is_ok_and(|installed| installed == current);
        if current != wanted && !was_ours {
            return Err(io::Error::other(
                "Firefox has a managed policy from another source. Existing policy kept.",
            ));
        }
    }

    let gid = install_group()?;
    let ports = |ports: &mut dyn Iterator<Item = u16>| {
        ports
            .map(|port| port.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let web_ports = ports(&mut web::ports());
    let https_ports = ports(&mut web::HTTPS_PORTS.iter().copied());
    let template = |text: &str| {
        text.replace("@GID@", &gid.to_string())
            .replace("@UID@", &uid.to_string())
            .replace("@USER@", user)
            .replace("@PSI_DIR@", &psi_dir)
            .replace("@SOCKS@", &socks.to_string())
            .replace("@HTTP@", &http.to_string())
            .replace("@WEB_PORTS@", &web_ports)
            .replace("@HTTPS_PORTS@", &https_ports)
    };
    let dir = Path::new(ETC);
    fs::create_dir_all(dir)?;
    let rules = dir.join("rules.nft");
    write(
        &dir.join("rules.nft.check"),
        template(include_str!("../assets/rules.nft.in")).as_bytes(),
        0o644,
    )?;
    command(
        "/usr/sbin/nft",
        &["--check", "-f", "/etc/claude-at/rules.nft.check"],
    )?;
    raw_firewall::protect(socks, http, gid)?;
    fs::rename(dir.join("rules.nft.check"), rules)?;
    write(&dir.join("dnsmasq.conf"), dns_conf().as_bytes(), 0o644)?;
    command(
        "/usr/sbin/dnsmasq",
        &["--test", "--conf-file=/etc/claude-at/dnsmasq.conf"],
    )?;
    write(&dir.join("domains.txt"), DOMAINS.as_bytes(), 0o644)?;
    write(
        Path::new(MANIFEST),
        json!({ "user": user, "firefox": manage_firefox })
            .to_string()
            .as_bytes(),
        0o644,
    )?;
    write(Path::new(BIN), &fs::read(env::current_exe()?)?, 0o755)?;
    let domains = entries()
        .map(|entry| format!("~{}", entry.trim_start_matches('=')))
        .collect::<Vec<_>>()
        .join(" ");
    for (name, unit) in [
        (UNIT, template(include_str!("../assets/claude-at.service"))),
        (
            FIREWALL,
            template(include_str!("../assets/claude-at-firewall.service")),
        ),
        (
            DNS,
            include_str!("../assets/claude-at-dns.service").to_owned(),
        ),
        (
            LINK,
            include_str!("../assets/claude-at-dns-link.service").replace("@DNS_DOMAINS@", &domains),
        ),
    ] {
        write(&Path::new(SYSTEMD).join(name), unit.as_bytes(), 0o644)?;
    }
    if manage_firefox {
        fs::create_dir_all(Path::new(FIREFOX).parent().unwrap())?;
        write(Path::new(FIREFOX), firefox.as_bytes(), 0o644)?;
        write(
            &dir.join("firefox-installed.json"),
            firefox.as_bytes(),
            0o644,
        )?;
    }
    command(
        "/usr/sbin/runuser",
        &["-u", user, "--", BIN, "--select-country", &region],
    )?;
    command("/usr/bin/systemctl", &["daemon-reload"])?;
    command("/usr/bin/systemctl", &["enable", UNIT, FIREWALL, DNS, LINK])?;
    command("/usr/bin/systemctl", &["restart", FIREWALL])?;
    command("/usr/bin/systemctl", &["restart", UNIT, LINK, DNS])?;
    Ok(())
}

pub fn wrap_cli() -> io::Result<()> {
    let dir = home()?.join(".local/bin");
    let cli = dir.join("claude");
    let backup = dir.join("claude-native");
    let target = fs::read_link(&cli).map_err(|_| {
        io::Error::other("Install native Claude Code in ~/.local/bin/claude first.")
    })?;
    if target == Path::new(BIN) {
        return Ok(());
    }
    let tmp = dir.join(format!(".claude-guard.{}", process::id()));
    symlink(&target, &tmp)?;
    fs::rename(&tmp, &backup)?;
    symlink(BIN, &tmp)?;
    fs::rename(tmp, cli)
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

pub fn unwrap_cli() -> io::Result<()> {
    let dir = home()?.join(".local/bin");
    let cli = dir.join("claude");
    let backup = dir.join("claude-native");
    if fs::read_link(&cli).ok().as_deref() == Some(Path::new(BIN)) {
        fs::rename(backup, cli)?;
    }
    Ok(())
}

pub fn remove() -> io::Result<()> {
    need_root()?;
    let install = json_file(Path::new(MANIFEST))?;
    let user = install["user"].as_str().ok_or(io::ErrorKind::InvalidData)?;
    command(
        "/usr/bin/systemctl",
        &["disable", "--now", FIREWALL, DNS, LINK, UNIT],
    )?;
    if command("/usr/sbin/nft", &["list", "table", "inet", "claude_at"]).is_ok() {
        command("/usr/sbin/nft", &["delete", "table", "inet", "claude_at"])?;
    }
    raw_firewall::remove()?;
    command(
        "/usr/sbin/runuser",
        &["-u", user, "--", BIN, "--unwrap-cli"],
    )?;
    command(
        "/usr/sbin/runuser",
        &["-u", user, "--", BIN, "--remove-policy"],
    )?;
    let original = Path::new(ETC).join("firefox-installed.json");
    if original.is_file()
        && Path::new(FIREFOX).is_file()
        && json_file(&original)? == json_file(Path::new(FIREFOX))?
    {
        remove_file(Path::new(FIREFOX))?;
    }
    for file in [
        "dnsmasq.conf",
        "domains.txt",
        "rules.nft",
        "rules.nft.check",
        "firefox-installed.json",
        "install.json",
    ] {
        remove_file(&Path::new(ETC).join(file))?;
    }
    for unit in [UNIT, FIREWALL, DNS, LINK] {
        remove_file(&Path::new(SYSTEMD).join(unit))?;
    }
    command("/usr/bin/systemctl", &["daemon-reload"])?;
    remove_file(Path::new(BIN))
}

pub fn remove_policy() -> io::Result<()> {
    remove_file(&home()?.join("psiphon").join(POLICY_FILE))
}
