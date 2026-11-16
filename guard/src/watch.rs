use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

pub const POLICY_FILE: &str = "claude-country.json";

pub struct Changes(File);

impl Changes {
    pub fn new(dir: &Path) -> io::Result<Self> {
        let dir = CString::new(dir.as_os_str().as_encoded_bytes())?;
        let mask = libc::IN_MODIFY
            | libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF;
        // The kernel copies the pathname and returns a watch identifier.
        let fd = own(unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) })?;
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), dir.as_ptr(), mask) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(File::from(fd)))
    }

    // A config edit, lost events and process exit require a fresh proof even
    // if the final snapshot looks unchanged after several events coalesced.
    pub fn wait(&mut self, pid: Option<u32>) -> io::Result<bool> {
        let process = if let Some(pid) = pid {
            match pidfd(pid) {
                Ok(fd) => Some(fd),
                Err(err) if err.raw_os_error() == Some(libc::ESRCH) => return Ok(true),
                Err(err) => return Err(err),
            }
        } else {
            None
        };
        let mut poll = [
            libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: process.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // Both file descriptors remain owned for the duration of poll.
            if unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as _, -1) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let mut changed = poll[1].revents != 0;
            if poll[0].revents & libc::POLLIN != 0 {
                let mut buf = [0; 8192];
                let len = self.0.read(&mut buf)?;
                for (mask, name) in notifications(&buf[..len]) {
                    if mask & (libc::IN_MOVE_SELF | libc::IN_DELETE_SELF | libc::IN_IGNORED) != 0 {
                        return Err(io::Error::other("Psiphon directory watch was lost"));
                    }
                    changed |= mask & libc::IN_Q_OVERFLOW != 0
                        || name == b"client.conf"
                        || name == POLICY_FILE.as_bytes();
                }
            } else if poll[0].revents != 0 {
                return Err(io::Error::other("Psiphon directory could not be watched"));
            }
            return Ok(changed);
        }
    }
}

fn notifications(mut bytes: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    std::iter::from_fn(move || {
        let header = bytes.get(..16)?;
        let mask = u32::from_ne_bytes(header[4..8].try_into().ok()?);
        let size = u32::from_ne_bytes(header[12..16].try_into().ok()?) as usize;
        let name = bytes.get(16..16 + size)?;
        bytes = bytes.get(16 + size..)?;
        Some((mask, name.split(|byte| *byte == 0).next()?))
    })
}

fn own(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Success of a descriptor-creating syscall transfers ownership to us.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn pidfd(pid: u32) -> io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::ErrorKind::InvalidInput)?;
    // A pidfd remains associated with the same process across PID reuse.
    own(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as libc::c_int)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::{self, Command};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn test_watch(name: &str, pid: Option<u32>, mutate: impl FnOnce(&Path)) {
        let dir = std::env::temp_dir().join(format!("claude-at-{}-{name}", process::id()));
        fs::create_dir(&dir).unwrap();
        let mut watch = Changes::new(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            loop {
                match watch.wait(pid) {
                    Ok(false) => {}
                    result => {
                        tx.send(result).unwrap();
                        break;
                    }
                }
            }
        });
        mutate(&dir);
        let answer = rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
        worker.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert!(answer);
    }

    #[test]
    fn atomic_country_file_replacement_wakes_the_guard() {
        test_watch("policy", None, |dir| {
            let staged = dir.join("new-policy");
            fs::write(&staged, b"{}").unwrap();
            fs::rename(staged, dir.join(POLICY_FILE)).unwrap();
        });
    }

    #[test]
    fn process_exit_wakes_the_guard_without_changes_to_log_or_pid_file() {
        let mut child = Command::new("/usr/bin/sleep").arg("0.1").spawn().unwrap();
        test_watch("exit", Some(child.id()), |_| {});
        child.wait().unwrap();
    }
}
