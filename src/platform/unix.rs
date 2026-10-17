//! Unix: socket endpoint, detached launch, and process groups.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tokio::net::{UnixListener, UnixStream};

pub type Stream = UnixStream;
pub type ClientStream = UnixStream;

fn socket_path(home: &Path) -> PathBuf {
    home.join("daemon").join("sock")
}

/// On unix the daemon decides, by comparing the caller with itself (see `more_confined`): a
/// whole container may look confined, and only the daemon knows the baseline.
pub fn self_sandboxed() -> bool {
    false
}

/// Whether `pid` is more confined than this process, i.e. inside an agent's sandbox. Comparing
/// with ourselves keeps a container that confines everything usable.
#[cfg(target_os = "linux")]
fn more_confined(pid: libc::pid_t) -> bool {
    // Landlock, seccomp and bubblewrap all need no_new_privs, which children inherit; every
    // seccomp layer adds a filter.
    let read = |pid: &str| -> Option<(u32, u32)> {
        let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let field = |name: &str| {
            s.lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0)
        };
        Some((field("NoNewPrivs:"), field("Seccomp_filters:")))
    };
    match (read(&pid.to_string()), read("self")) {
        (Some(peer), Some(own)) => peer.0 > own.0 || peer.1 > own.1,
        _ => true,
    }
}

/// Seatbelt, as used by `sandbox-exec` and the App Sandbox.
#[cfg(target_os = "macos")]
fn more_confined(pid: libc::pid_t) -> bool {
    unsafe extern "C" {
        // libsystem_sandbox; without an operation it tells whether the process is sandboxed.
        fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            kind: libc::c_int,
            ...
        ) -> libc::c_int;
    }
    let sandboxed = |pid| unsafe { sandbox_check(pid, std::ptr::null(), 0) != 0 };
    sandboxed(pid) && !sandboxed(unsafe { libc::getpid() })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn more_confined(_: libc::pid_t) -> bool {
    false
}

/// Executable names of our ancestor processes, nearest first. Linux only; elsewhere empty.
pub fn ancestor_names() -> Vec<String> {
    let mut names = Vec::new();
    let mut pid = std::process::id();
    for _ in 0..32 {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        // pid (comm) state ppid ...; comm may contain spaces, so split after the last ')'.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            break;
        };
        let Some(ppid) = rest
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse::<u32>().ok())
        else {
            break;
        };
        if ppid <= 1 {
            break;
        }
        match std::fs::read_to_string(format!("/proc/{ppid}/comm")) {
            Ok(comm) => names.push(comm.trim().to_string()),
            Err(_) => break,
        }
        pid = ppid;
    }
    names
}

pub struct Listener(UnixListener);

impl Listener {
    pub fn bind(home: &Path) -> io::Result<Listener> {
        let dir = home.join("daemon");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let path = socket_path(home);
        // Only the daemon holding the lock gets here, so a leftover socket is stale.
        let _ = std::fs::remove_file(&path);
        Ok(Listener(UnixListener::bind(&path)?))
    }

    pub async fn accept(&mut self) -> io::Result<(Stream, Peer)> {
        loop {
            let (stream, _) = self.0.accept().await?;
            let cred = stream.peer_cred()?;
            if cred.uid() == unsafe { libc::geteuid() } {
                let restricted = cred.pid().is_some_and(more_confined);
                return Ok((stream, Peer { restricted }));
            }
        }
    }
}

pub struct Peer {
    restricted: bool,
}

impl Peer {
    /// Whether the client is more confined than the daemon, i.e. inside an agent's sandbox.
    pub fn restricted(&self) -> bool {
        self.restricted
    }
}

pub async fn connect(home: &Path) -> io::Result<ClientStream> {
    UnixStream::connect(socket_path(home)).await
}

pub fn spawn_detached(exe: &Path, args: &[&str]) -> io::Result<&'static str> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok("setsid")
}

/// Every process of one sub-agent, as a process group.
pub struct Job(Mutex<Option<i32>>);

impl Job {
    pub fn new() -> io::Result<Job> {
        Ok(Job(Mutex::new(None)))
    }

    pub fn spawn(&self, cmd: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
        cmd.process_group(0);
        let child = cmd.spawn()?;
        *self.0.lock().unwrap() = child.id().map(|p| p as i32);
        Ok(child)
    }

    pub fn kill(&self) {
        if let Some(pgid) = *self.0.lock().unwrap() {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.kill();
    }
}
