mod gate;
mod install;
mod proxy;
mod watch;

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

const STATUS: &str = "/run/claude-at/status.sock";
const PROXY: &str = "http://127.0.0.1:18082";

fn allowed_country() -> Option<String> {
    let mut status = UnixStream::connect(STATUS).ok()?;
    let mut answer = [0; 3];
    status.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    status.read_exact(&mut answer).ok()?;
    (answer[2] == b'\n' && answer[..2].iter().all(u8::is_ascii_uppercase))
        .then(|| String::from_utf8_lossy(&answer[..2]).into_owned())
}

fn port(arg: Option<std::ffi::OsString>, default: u16) -> io::Result<u16> {
    match arg {
        None => Ok(default),
        Some(arg) => arg
            .to_str()
            .and_then(|s| s.parse().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| io::ErrorKind::InvalidInput.into()),
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1).peekable();
    if args.next_if_eq(OsStr::new("--install")).is_some() {
        let user = args
            .next()
            .and_then(|s| s.into_string().ok())
            .ok_or(io::ErrorKind::InvalidInput)?;
        let country = args
            .next()
            .and_then(|s| s.into_string().ok())
            .ok_or(io::ErrorKind::InvalidInput)?;
        let firefox = args.next_if_eq(OsStr::new("--firefox")).is_some();
        if args.next().is_some() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        return install::install(&user, &country, firefox);
    }
    if args.next_if_eq(OsStr::new("--uninstall")).is_some() {
        return install::remove();
    }
    if args.next_if_eq(OsStr::new("--select-country")).is_some() {
        let country = args
            .next()
            .and_then(|s| s.into_string().ok())
            .ok_or(io::ErrorKind::InvalidInput)?;
        return install::select(&country);
    }
    if args.next_if_eq(OsStr::new("--wrap-cli")).is_some() {
        return install::wrap_cli();
    }
    if args.next_if_eq(OsStr::new("--unwrap-cli")).is_some() {
        return install::unwrap_cli();
    }
    if args.next_if_eq(OsStr::new("--remove-policy")).is_some() {
        return install::remove_policy();
    }
    if args.next_if_eq(OsStr::new("--serve")).is_some() {
        let dir = args.next().ok_or(io::ErrorKind::InvalidInput)?;
        let socks = port(args.next(), 1081)?;
        let http = port(args.next(), 8081)?;
        let gate = gate::start(Path::new(&dir), STATUS, socks, http)?;
        return tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?
            .block_on(proxy::serve(gate));
    }
    let status_only = args.next_if_eq(OsStr::new("--vpn-status")).is_some();
    let Some(country) = allowed_country() else {
        return Err(io::Error::other(
            "Claude is blocked. Connect Psiphon to the country selected for Claude.",
        ));
    };
    if status_only {
        println!("Psiphon: {country}");
        return Ok(());
    }
    let bin = env::home_dir()
        .ok_or(io::ErrorKind::NotFound)?
        .join(".local/bin");
    let wrapper = env::current_exe()?;
    let native = match fs::canonicalize(bin.join("claude")) {
        Ok(installed) if installed != wrapper => installed,
        _ => fs::canonicalize(bin.join("claude-native"))?,
    };
    let mut command = Command::new(native);
    command.args(args);
    for name in [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        command.env_remove(name);
    }
    for name in [
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "https_proxy",
        "http_proxy",
        "all_proxy",
    ] {
        command.env(name, PROXY);
    }
    for name in ["NO_PROXY", "no_proxy"] {
        command.env(name, "localhost,127.0.0.1,::1");
    }
    Err(command.exec())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
