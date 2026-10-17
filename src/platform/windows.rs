//! Windows: named pipe endpoint, token checks, detached launch, and job objects.

use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::time::Duration;
use std::{mem, slice};

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;

pub type Stream = NamedPipeServer;
pub type ClientStream = NamedPipeClient;

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

// ---- tokens ----

struct Token(HANDLE);

impl Token {
    fn process() -> io::Result<Token> {
        let mut h = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Token(h))
    }

    fn info(&self, class: TOKEN_INFORMATION_CLASS) -> Vec<u64> {
        let mut len = 0u32;
        unsafe { GetTokenInformation(self.0, class, null_mut(), 0, &mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let ok =
            unsafe { GetTokenInformation(self.0, class, buf.as_mut_ptr().cast(), len, &mut len) };
        if ok == 0 { Vec::new() } else { buf }
    }

    fn user_sid(&self) -> io::Result<String> {
        let buf = self.info(TokenUser);
        if buf.is_empty() {
            return Err(io::Error::last_os_error());
        }
        sid_string(unsafe { &*(buf.as_ptr() as *const TOKEN_USER) }.User.Sid)
    }

    fn logon_sid(&self) -> io::Result<String> {
        let buf = self.info(TokenLogonSid);
        if buf.is_empty() {
            return Err(io::Error::last_os_error());
        }
        let groups = unsafe { &*(buf.as_ptr() as *const TOKEN_GROUPS) };
        let items =
            unsafe { slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
        match items.first() {
            Some(g) => sid_string(g.Sid),
            None => Err(io::Error::other("token has no logon SID")),
        }
    }

    fn restricted(&self) -> bool {
        unsafe { IsTokenRestricted(self.0) != 0 }
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut s: *mut u16 = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = (0..).take_while(|&i| unsafe { *s.add(i) } != 0).count();
    let out = String::from_utf16_lossy(unsafe { slice::from_raw_parts(s, len) });
    unsafe { LocalFree(s.cast()) };
    Ok(out)
}

/// True inside a restricted-token sandbox such as Codex's unelevated Windows sandbox.
pub fn self_sandboxed() -> bool {
    Token::process().map(|t| t.restricted()).unwrap_or(false)
}

/// Executable names of our ancestor processes, nearest first.
pub fn ancestor_names() -> Vec<String> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut procs = std::collections::HashMap::new();
    let mut entry: PROCESSENTRY32W = unsafe { mem::zeroed() };
    entry.dwSize = mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(snap, &mut entry) } != 0;
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
        procs.insert(entry.th32ProcessID, (entry.th32ParentProcessID, name));
        more = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snap) };
    let mut names = Vec::new();
    let mut pid = std::process::id();
    // Bounded: parent IDs can be stale and point at an unrelated, reused PID.
    for _ in 0..32 {
        let Some((parent, _)) = procs.get(&pid) else {
            break;
        };
        let Some((_, name)) = procs.get(parent) else {
            break;
        };
        names.push(name.clone());
        pid = *parent;
    }
    names
}

// ---- endpoint ----

fn pipe_name(home: &Path) -> io::Result<String> {
    let user = Token::process()?.user_sid()?;
    let key = format!("{user}|{}", home.to_string_lossy().to_lowercase());
    Ok(format!(
        r"\\.\pipe\axiopera-{:016x}",
        crate::paths::fnv1a(key.as_bytes())
    ))
}

/// Owned security descriptor for the pipe: the current user, plus the current logon session so
/// that a restricted-token sandbox of the same user passes the second (restricting-SID) check.
struct Security(PSECURITY_DESCRIPTOR);

unsafe impl Send for Security {}
unsafe impl Sync for Security {}

impl Security {
    fn new() -> io::Result<Security> {
        let token = Token::process()?;
        let sddl = format!(
            "D:P(A;;GA;;;{})(A;;GA;;;{})",
            token.user_sid()?,
            token.logon_sid()?
        );
        let mut sd = null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(&sddl).as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Security(sd))
    }

    fn create(&self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut sa as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
                )
        }
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

pub struct Listener {
    name: String,
    security: Security,
    next: NamedPipeServer,
}

impl Listener {
    pub fn bind(home: &Path) -> io::Result<Listener> {
        let name = pipe_name(home)?;
        let security = Security::new()?;
        // FIRST_PIPE_INSTANCE: fail instead of joining a pipe someone else already created.
        let next = security.create(&name, true)?;
        Ok(Listener {
            name,
            security,
            next,
        })
    }

    pub async fn accept(&mut self) -> io::Result<(Stream, Peer)> {
        self.next.connect().await?;
        let fresh = self.security.create(&self.name, false)?;
        let conn = mem::replace(&mut self.next, fresh);
        let peer = Peer(conn.as_raw_handle() as isize);
        Ok((conn, peer))
    }
}

/// The client end of an accepted connection, for token checks.
pub struct Peer(isize);

impl Peer {
    /// Whether the client runs with a restricted token. Call only after reading from the pipe:
    /// Windows refuses impersonation before the client has written anything.
    pub fn restricted(&self) -> bool {
        let pipe = self.0 as HANDLE;
        if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
            // Fail closed: an unknown caller is treated as sandboxed.
            return true;
        }
        let mut h = null_mut();
        let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut h) } != 0;
        unsafe { RevertToSelf() };
        if !opened {
            return true;
        }
        Token(h).restricted()
    }
}

pub async fn connect(home: &Path) -> io::Result<ClientStream> {
    let name = pipe_name(home)?;
    for _ in 0..40 {
        match ClientOptions::new().open(&name) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            other => return other,
        }
    }
    Err(io::Error::other("daemon pipe stayed busy"))
}

// ---- launching the daemon ----

/// Quote one argument for CreateProcess / CommandLineToArgvW.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// CreateProcessW without inheriting any handle: an inherited pipe from the host's shell would
/// keep the host's command from ever seeing EOF.
fn create_detached(cmdline: &str, flags: u32) -> io::Result<()> {
    let mut cmd = wide(cmdline);
    let mut si: STARTUPINFOW = unsafe { mem::zeroed() };
    si.cb = mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(
            null(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            0,
            flags,
            null(),
            null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    Ok(())
}

/// Start a process that outlives the calling host's shell command. Returns how it was started.
///
/// Claude runs shell commands outside any job; Codex puts each in a job that allows breakaway;
/// Grok's job forbids breakaway, and only a process created by WMI escapes it.
pub fn spawn_detached(exe: &Path, args: &[&str]) -> io::Result<&'static str> {
    let cmdline = std::iter::once(quote(&exe.to_string_lossy()))
        .chain(args.iter().map(|a| quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    let mut in_job = 0;
    unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) };
    if in_job == 0 {
        create_detached(&cmdline, flags)?;
        return Ok("detached");
    }
    match create_detached(&cmdline, flags | CREATE_BREAKAWAY_FROM_JOB) {
        Ok(()) => Ok("breakaway"),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            wmi_create(&cmdline)?;
            Ok("wmi")
        }
        Err(e) => Err(e),
    }
}

/// Win32_Process.Create: the new process is a child of the WMI provider host, outside our job.
/// Note it gets the user's registry environment, not ours.
fn wmi_create(cmdline: &str) -> io::Result<()> {
    let script = format!(
        "$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine='{}'}}; exit $r.ReturnValue",
        cmdline.replace('\'', "''")
    );
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let status = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &base64(&utf16),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "Win32_Process.Create failed: {status}"
        )));
    }
    Ok(())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() {
                T[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

// ---- job objects ----

/// Every process of one sub-agent. Dropping the job kills whatever is still in it.
pub struct Job(isize);

impl Job {
    pub fn new() -> io::Result<Job> {
        let h = unsafe { CreateJobObjectW(null(), null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Job(h as isize);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
        // No BREAKAWAY_OK: nothing the agent starts may leave the job.
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                mem::size_of_val(&limits) as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Spawn suspended, put the process in the job, then resume it, so no grandchild can start
    /// before the job owns the tree.
    pub fn spawn(&self, cmd: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
        cmd.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        let mut child = cmd.spawn()?;
        let handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("child has no handle"))?;
        if unsafe { AssignProcessToJobObject(self.0 as HANDLE, handle as HANDLE) } == 0 {
            let e = io::Error::last_os_error();
            let _ = child.start_kill();
            return Err(e);
        }
        resume_threads(child.id().unwrap_or(0))?;
        Ok(child)
    }

    pub fn kill(&self) {
        unsafe { TerminateJobObject(self.0 as HANDLE, 1) };
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0 as HANDLE) };
    }
}

fn resume_threads(pid: u32) -> io::Result<()> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry: THREADENTRY32 = unsafe { mem::zeroed() };
    entry.dwSize = mem::size_of::<THREADENTRY32>() as u32;
    let mut more = unsafe { Thread32First(snap, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            let t = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !t.is_null() {
                unsafe {
                    ResumeThread(t);
                    CloseHandle(t);
                }
            }
        }
        more = unsafe { Thread32Next(snap, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snap) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_follows_argv_rules() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(""), "\"\"");
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi!"), "aGkh");
        assert_eq!(base64(b"a"), "YQ==");
    }
}
