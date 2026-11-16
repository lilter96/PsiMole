#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod platform;
#[cfg(windows)]
mod runtime;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let dir = platform::data_dir()?;
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--watch-proxy") => {
            let pid = args.next().ok_or("missing process id")?.parse()?;
            let started = args
                .next()
                .ok_or("missing process creation time")?
                .parse()?;
            platform::watch_proxy(&dir, pid, started)?;
        }
        Some("--quit-if-running") => platform::quit_if_running(&dir)?,
        Some("--restore-proxy") => {
            if let Some(_lock) = platform::try_instance(&dir)? {
                platform::restore_proxy(&dir)?;
            }
        }
        Some("--version") => println!("PsiMole {}", env!("CARGO_PKG_VERSION")),
        Some(_) => return Err("Unknown PsiMole command".into()),
        None => {
            let commands = platform::WindowCommands::new(&dir)?;
            if let Some(_lock) = platform::try_instance(&dir)? {
                let log = dir.join("startup-error.txt");
                let _ = std::fs::remove_file(&log);
                if let Err(error) = ui::run(dir, commands) {
                    let _ = std::fs::write(log, error.to_string());
                    return Err(error.into());
                }
            } else {
                platform::signal(&commands.show)?;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if std::env::args_os().len() > 1 {
                eprintln!("{error}");
            } else {
                platform::show_error(&error);
            }
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    use psimole::speed::{Route, measure};
    use std::num::NonZeroU16;
    use std::process::ExitCode;

    let args: Vec<_> = std::env::args().skip(1).collect();
    let route = match args.as_slice() {
        [command] if command == "--measure-speed" => Route::Proxy(NonZeroU16::new(8081).unwrap()),
        [command, route] if command == "--measure-speed" => {
            if route == "direct" {
                Route::Direct
            } else if let Ok(port) = route.parse() {
                Route::Proxy(port)
            } else {
                eprintln!("Use a valid loopback HTTP proxy port or 'direct'.");
                return ExitCode::FAILURE;
            }
        }
        _ => {
            eprintln!(
                "Use the PsiMole GNOME extension on Linux. Run psimole --measure-speed [PORT|direct] to measure a connection."
            );
            return if args.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
    };
    match measure(route, || true) {
        Ok(result) => {
            println!(
                "{:.2} Mbps down, {:.2} Mbps up",
                result.down_mbps, result.up_mbps
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("Speed test failed: {error}");
            ExitCode::FAILURE
        }
    }
}
