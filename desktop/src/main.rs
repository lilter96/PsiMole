#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod platform;
#[cfg(windows)]
mod runtime;
#[cfg(windows)]
mod speed;
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
fn main() {
    eprintln!("Use the PsiMole GNOME extension on Linux.");
}
