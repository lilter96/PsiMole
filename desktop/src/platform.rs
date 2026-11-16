use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::ffi::{OsStr, c_void};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, FILETIME, GlobalFree, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
use windows_sys::Win32::Networking::WinInet::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SW_RESTORE, SetForegroundWindow, ShowWindowAsync,
};

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain([0]).collect()
}

fn handle(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() {
        Err(io::Error::last_os_error())
    } else {
        // The Win32 creation APIs transfer ownership of every non-null handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    }
}

fn win32(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn data_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::other("LOCALAPPDATA is missing"))?;
    let path = PathBuf::from(base).join("PsiMole");
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

fn instance_file(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("instance.lock"))
}

pub fn try_instance(dir: &Path) -> io::Result<Option<File>> {
    let file = instance_file(dir)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

fn event(dir: &Path, suffix: &str) -> io::Result<OwnedHandle> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    dir.as_os_str().hash(&mut hasher);
    let name = wide(format!("Local\\PsiMole.{:016x}.{suffix}", hasher.finish()));
    // Auto-reset events retain a single request until a listener starts waiting.
    handle(unsafe { CreateEventW(null(), 0, 0, name.as_ptr()) })
}

pub struct WindowCommands {
    pub show: OwnedHandle,
    pub quit: OwnedHandle,
}

impl WindowCommands {
    pub fn new(dir: &Path) -> io::Result<Self> {
        Ok(Self {
            show: event(dir, "show")?,
            quit: event(dir, "quit")?,
        })
    }
}

pub fn signal(event: &OwnedHandle) -> io::Result<()> {
    win32(unsafe { SetEvent(event.as_raw_handle()) })
}

pub fn quit_if_running(dir: &Path) -> io::Result<()> {
    if let Some(_instance) = try_instance(dir)? {
        return restore_proxy(dir);
    }
    let commands = WindowCommands::new(dir)?;
    signal(&commands.quit)?;
    // Used by the installer to wait for the old executable and proxy to be released.
    for _ in 0..100 {
        if let Some(_instance) = try_instance(dir)? {
            return restore_proxy(dir);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "PsiMole is still running",
    ))
}

pub fn listen_for_window(commands: WindowCommands, hwnd: isize, quit: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let handles = [commands.show.as_raw_handle(), commands.quit.as_raw_handle()];
        loop {
            match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) } {
                WAIT_OBJECT_0 => show_window(hwnd),
                result if result == WAIT_OBJECT_0 + 1 => quit(),
                _ => return,
            }
        }
    });
}

pub fn show_window(hwnd: isize) {
    let hwnd = hwnd as _;
    unsafe {
        ShowWindowAsync(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
    }
}

pub fn default_russian() -> bool {
    // The low 10 bits of LANGID contain the primary language.
    unsafe { GetUserDefaultUILanguage() & 0x3ff == 0x19 }
}

pub struct EngineJob {
    job: OwnedHandle,
    process: OwnedHandle,
}

impl EngineJob {
    pub fn stop(self) -> io::Result<()> {
        drop(self.job);
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 5_000) } {
            WAIT_OBJECT_0 => Ok(()),
            _ => Err(io::Error::other("Psiphon did not release its local ports")),
        }
    }
}

pub fn attach_job(child: &Child) -> io::Result<EngineJob> {
    let process = child.as_handle().try_clone_to_owned()?;
    let job = handle(unsafe { CreateJobObjectW(null(), null()) })?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    win32(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    })?;
    win32(unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) })?;
    Ok(EngineJob { job, process })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct ProxyOptions {
    flags: u32,
    server: Vec<u16>,
}

fn proxy_list(options: &mut [INTERNET_PER_CONN_OPTIONW; 2]) -> INTERNET_PER_CONN_OPTION_LISTW {
    INTERNET_PER_CONN_OPTION_LISTW {
        dwSize: size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32,
        pOptions: options.as_mut_ptr(),
        dwOptionCount: options.len() as u32,
        ..Default::default()
    }
}

fn proxy_options() -> io::Result<ProxyOptions> {
    let mut options = [
        INTERNET_PER_CONN_OPTIONW {
            dwOption: INTERNET_PER_CONN_FLAGS,
            ..Default::default()
        },
        INTERNET_PER_CONN_OPTIONW {
            dwOption: INTERNET_PER_CONN_PROXY_SERVER,
            ..Default::default()
        },
    ];
    let mut list = proxy_list(&mut options);
    let result = win32(unsafe {
        InternetQueryOptionW(
            null(),
            INTERNET_OPTION_PER_CONNECTION_OPTION,
            (&mut list as *mut INTERNET_PER_CONN_OPTION_LISTW).cast(),
            &mut (size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32),
        )
    });
    // WinINet allocates the returned NUL-terminated string with GlobalAlloc.
    let server = unsafe {
        let raw = options[1].Value.pszValue;
        if raw.is_null() {
            Vec::new()
        } else {
            let mut length = 0;
            while *raw.add(length) != 0 {
                length += 1;
            }
            let value = std::slice::from_raw_parts(raw, length).to_vec();
            GlobalFree(raw.cast());
            value
        }
    };
    result?;
    Ok(ProxyOptions {
        flags: unsafe { options[0].Value.dwValue },
        server,
    })
}

fn set_proxy(options: &ProxyOptions) -> io::Result<()> {
    let mut server = options.server.clone();
    server.push(0);
    let mut entries = [
        INTERNET_PER_CONN_OPTIONW {
            dwOption: INTERNET_PER_CONN_FLAGS,
            Value: INTERNET_PER_CONN_OPTIONW_0 {
                dwValue: options.flags,
            },
        },
        INTERNET_PER_CONN_OPTIONW {
            dwOption: INTERNET_PER_CONN_PROXY_SERVER,
            Value: INTERNET_PER_CONN_OPTIONW_0 {
                pszValue: server.as_mut_ptr(),
            },
        },
    ];
    let list = proxy_list(&mut entries);
    win32(unsafe {
        InternetSetOptionW(
            null(),
            INTERNET_OPTION_PER_CONNECTION_OPTION,
            (&list as *const INTERNET_PER_CONN_OPTION_LISTW).cast(),
            list.dwSize,
        )
    })?;
    unsafe {
        InternetSetOptionW(null(), INTERNET_OPTION_SETTINGS_CHANGED, null(), 0);
        InternetSetOptionW(null(), INTERNET_OPTION_REFRESH, null(), 0);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Owner {
    pid: u32,
    started: u64,
}

fn start_time(process: HANDLE) -> io::Result<u64> {
    let mut created = FILETIME::default();
    win32(unsafe {
        GetProcessTimes(
            process,
            &mut created,
            &mut FILETIME::default(),
            &mut FILETIME::default(),
            &mut FILETIME::default(),
        )
    })?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

#[derive(Deserialize, Serialize)]
struct ProxyLease {
    owner: Owner,
    original: ProxyOptions,
    applied: ProxyOptions,
}

const LEASE: &str = "proxy-lease.json";
const RUN_ONCE: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
const RECOVERY: &str = "!PsiMoleRestoreProxy";

fn restore_on_logon() -> io::Result<()> {
    let command = wide(format!(
        r#""{}" --restore-proxy"#,
        std::env::current_exe()?.display()
    ));
    let result = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            wide(RUN_ONCE).as_ptr(),
            wide(RECOVERY).as_ptr(),
            REG_SZ,
            command.as_ptr().cast(),
            size_of_val(command.as_slice()) as u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result as i32))
    }
}

fn clear_logon_recovery() -> io::Result<()> {
    let result = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            wide(RUN_ONCE).as_ptr(),
            wide(RECOVERY).as_ptr(),
        )
    };
    if matches!(result, 0 | ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result as i32))
    }
}

fn restore_options(current: &ProxyOptions, lease: &ProxyLease) -> Option<ProxyOptions> {
    if current == &lease.applied {
        Some(lease.original.clone())
    } else if current.server == lease.applied.server {
        // Preserve an external change to the mode and still remove our stale endpoint.
        Some(ProxyOptions {
            flags: current.flags,
            server: lease.original.server.clone(),
        })
    } else {
        None
    }
}

pub fn restore_proxy(dir: &Path) -> io::Result<()> {
    let path = dir.join(LEASE);
    let Some(lease) = read_json::<ProxyLease>(&path)? else {
        return Ok(());
    };
    if let Some(original) = restore_options(&proxy_options()?, &lease) {
        set_proxy(&original)?;
    }
    clear_logon_recovery()?;
    fs::remove_file(path)
}

pub struct SystemProxy {
    dir: PathBuf,
    watcher: Option<Child>,
    owner: Owner,
}

impl SystemProxy {
    pub fn new(dir: PathBuf) -> io::Result<Self> {
        restore_proxy(&dir)?;
        let owner = Owner {
            pid: std::process::id(),
            started: start_time(unsafe { GetCurrentProcess() })?,
        };
        Ok(Self {
            dir,
            watcher: None,
            owner,
        })
    }

    pub fn apply(&mut self, port: u16) -> io::Result<()> {
        let applied = ProxyOptions {
            flags: PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY,
            server: format!("127.0.0.1:{port}").encode_utf16().collect(),
        };
        let path = self.dir.join(LEASE);
        let current = proxy_options()?;
        if let Some(lease) = read_json::<ProxyLease>(&path)? {
            if current == lease.applied && lease.applied == applied {
                return Ok(());
            }
            return Err(io::Error::other("Windows proxy settings changed"));
        }
        if self.watcher.is_none() {
            self.watcher = Some(
                Command::new(std::env::current_exe()?)
                    .arg("--watch-proxy")
                    .arg(self.owner.pid.to_string())
                    .arg(self.owner.started.to_string())
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()?,
            );
        }
        write_json(
            &path,
            &ProxyLease {
                owner: self.owner,
                original: current,
                applied: applied.clone(),
            },
        )?;
        if let Err(error) = restore_on_logon().and_then(|()| set_proxy(&applied)) {
            let _ = restore_proxy(&self.dir);
            return Err(error);
        }
        Ok(())
    }

    pub fn restore(&self) -> io::Result<()> {
        restore_proxy(&self.dir)
    }
}

pub fn watch_proxy(dir: &Path, pid: u32, started: u64) -> io::Result<()> {
    let owner = Owner { pid, started };
    if let Ok(process) = handle(unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    }) && start_time(process.as_raw_handle())? == started
    {
        let result = unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
        if result != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
    }
    let Some(_instance) = try_instance(dir)? else {
        return Ok(());
    };
    if read_json::<ProxyLease>(&dir.join(LEASE))?.is_some_and(|lease| lease.owner == owner) {
        restore_proxy(dir)?;
    }
    Ok(())
}

pub fn show_error(error: &dyn std::fmt::Display) {
    let caption = wide("PsiMole");
    let message = wide(error.to_string());
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    unsafe {
        MessageBoxW(
            null_mut::<c_void>(),
            message.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_pac_flags_and_do_not_overwrite_external_proxy() {
        let original = ProxyOptions {
            flags: PROXY_TYPE_DIRECT | PROXY_TYPE_AUTO_PROXY_URL,
            server: Vec::new(),
        };
        let applied = ProxyOptions {
            flags: PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY,
            server: vec![1],
        };
        let lease = ProxyLease {
            owner: Owner { pid: 0, started: 0 },
            original: original.clone(),
            applied: applied.clone(),
        };
        assert_eq!(restore_options(&applied, &lease), Some(original));
        assert_eq!(
            restore_options(
                &ProxyOptions {
                    flags: PROXY_TYPE_DIRECT,
                    server: vec![1]
                },
                &lease
            ),
            Some(ProxyOptions {
                flags: PROXY_TYPE_DIRECT,
                server: Vec::new()
            })
        );
        assert_eq!(
            restore_options(
                &ProxyOptions {
                    flags: PROXY_TYPE_PROXY,
                    server: vec![2]
                },
                &lease
            ),
            None
        );
    }

    #[test]
    fn one_instance_until_file_handle_closes() {
        let dir = tempfile::tempdir().unwrap();
        let first = try_instance(dir.path()).unwrap().unwrap();
        assert!(try_instance(dir.path()).unwrap().is_none());
        drop(first);
        assert!(try_instance(dir.path()).unwrap().is_some());
    }

    #[test]
    fn dropping_a_job_terminates_the_child() {
        let mut child = Command::new("cmd.exe")
            .args(["/C", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let job = attach_job(&child).unwrap();
        assert!(child.try_wait().unwrap().is_none());
        job.stop().unwrap();
        let _ = child.wait().unwrap();
    }

    #[test]
    #[ignore = "changes the Windows proxy temporarily; run in a disposable Windows session"]
    fn windows_proxy_recovery_after_crash() {
        struct Restore(ProxyOptions);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = set_proxy(&self.0);
                let _ = clear_logon_recovery();
            }
        }
        let original = Restore(proxy_options().unwrap());
        let applied = ProxyOptions {
            flags: PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY,
            server: "127.0.0.1:65535".encode_utf16().collect(),
        };
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("cmd.exe")
            .args(["/C", "pause"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let job = attach_job(&child).unwrap();
        let owner = Owner {
            pid: child.id(),
            started: start_time(child.as_raw_handle()).unwrap(),
        };
        let journal = dir.path().join(LEASE);
        write_json(
            &journal,
            &ProxyLease {
                owner,
                original: original.0.clone(),
                applied: applied.clone(),
            },
        )
        .unwrap();
        restore_on_logon().unwrap();
        set_proxy(&applied).unwrap();
        assert_eq!(proxy_options().unwrap(), applied);
        job.stop().unwrap();
        let _ = child.wait().unwrap();
        watch_proxy(dir.path(), owner.pid, owner.started).unwrap();
        assert_eq!(proxy_options().unwrap(), original.0);
        assert!(!journal.exists());
    }
}
