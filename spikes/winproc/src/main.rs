//! Windows process and IPC mechanics for the daemon design. See the subcommands in `main`.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::ptr::{null, null_mut};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, mem, process, slice, thread};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match a.as_slice() {
        // Where am I running: token restrictions and job limits.
        ["token"] => {
            println!("{}", process_report());
            Ok(())
        }
        // Start a detached heartbeat process that should outlive the host's shell command.
        ["launch", heartbeat, secs] => launch(heartbeat, secs),
        ["heartbeat", path, secs] => heartbeat(path, secs.parse().unwrap()),
        // Run a command line in a fresh kill-on-close job, then kill the whole job.
        ["job-run", secs, cmdline] => job_run(secs.parse().unwrap(), cmdline),
        // Named pipe server with a chosen DACL; reports each client's token.
        ["serve", name, dacl, count] => serve(name, dacl, count.parse().unwrap()),
        ["connect", name] => connect(name),
        _ => {
            eprintln!("usage: winproc token | launch <file> <secs> | job-run <secs> <cmdline> | serve <pipe> <dacl> <n> | connect <pipe>");
            process::exit(2)
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn check(ok: i32, what: &str) -> io::Result<()> {
    if ok == 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("{what}: {e}")));
    }
    Ok(())
}

// ---- token and job inspection ----

fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Vec<u64> {
    let mut len = 0u32;
    unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut len) };
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    let ok = unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) };
    if ok == 0 { Vec::new() } else { buf }
}

fn sid_string(sid: PSID) -> String {
    let mut s: *mut u16 = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
        return "?".into();
    }
    let len = (0..).take_while(|&i| unsafe { *s.add(i) } != 0).count();
    let out = String::from_utf16_lossy(unsafe { slice::from_raw_parts(s, len) });
    unsafe { LocalFree(s.cast()) };
    out
}

fn groups(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Vec<String> {
    let buf = token_info(token, class);
    if buf.is_empty() {
        return Vec::new();
    }
    let g = unsafe { &*(buf.as_ptr() as *const TOKEN_GROUPS) };
    let items = unsafe { slice::from_raw_parts(g.Groups.as_ptr(), g.GroupCount as usize) };
    items.iter().map(|x| sid_string(x.Sid)).collect()
}

fn user_sid(token: HANDLE) -> String {
    let buf = token_info(token, TokenUser);
    sid_string(unsafe { &*(buf.as_ptr() as *const TOKEN_USER) }.User.Sid)
}

fn integrity(token: HANDLE) -> u32 {
    let buf = token_info(token, TokenIntegrityLevel);
    if buf.is_empty() {
        return 0;
    }
    let sid = unsafe { &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL) }.Label.Sid;
    unsafe { *GetSidSubAuthority(sid, *GetSidSubAuthorityCount(sid) as u32 - 1) }
}

fn token_report(token: HANDLE) -> String {
    let restricted = unsafe { IsTokenRestricted(token) } != 0;
    let appcontainer = token_info(token, TokenIsAppContainer).first().map_or(0, |v| *v as u32);
    format!(
        "user={} restricted={restricted} integrity=0x{:x} appcontainer={appcontainer} logon={:?} restricting={:?}",
        user_sid(token),
        integrity(token),
        groups(token, TokenLogonSid),
        groups(token, TokenRestrictedSids),
    )
}

fn process_token() -> HANDLE {
    let mut token = null_mut();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    token
}

fn job_report() -> String {
    let mut in_job = 0;
    unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) };
    if in_job == 0 {
        return "job=none".into();
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
    let ok = unsafe {
        QueryInformationJobObject(
            null_mut(),
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            mem::size_of_val(&info) as u32,
            null_mut(),
        )
    };
    if ok == 0 {
        return format!("job=yes limits=unreadable({})", io::Error::last_os_error());
    }
    let f = info.BasicLimitInformation.LimitFlags;
    format!(
        "job=yes breakaway_ok={} silent_breakaway={} kill_on_close={}",
        f & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0,
        f & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK != 0,
        f & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0
    )
}

fn process_report() -> String {
    format!("pid={} {} {}", process::id(), token_report(process_token()), job_report())
}

// ---- detached launch ----

/// CreateProcessW without inheriting any handle: an inherited pipe from the host's shell would
/// keep the host's command from ever seeing EOF.
fn create_process(cmdline: &str, flags: u32) -> io::Result<PROCESS_INFORMATION> {
    let mut cmd = wide(cmdline);
    let mut si: STARTUPINFOW = unsafe { mem::zeroed() };
    si.cb = mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    let ok = unsafe {
        CreateProcessW(null(), cmd.as_mut_ptr(), null(), null(), 0, flags, null(), null(), &si, &mut pi)
    };
    // Keep the raw OS error: the caller falls back to WMI on ERROR_ACCESS_DENIED.
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pi)
}

fn launch(heartbeat: &str, secs: &str) -> io::Result<()> {
    let exe = env::current_exe()?;
    let cmdline = format!("\"{}\" heartbeat \"{heartbeat}\" {secs}", exe.display());
    let detached = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    let mut in_job = 0;
    unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) };
    let how = if in_job == 0 {
        create_process(&cmdline, detached)?;
        "detached"
    } else {
        match create_process(&cmdline, detached | CREATE_BREAKAWAY_FROM_JOB) {
            Ok(_) => "breakaway",
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                wmi_create(&cmdline)?;
                "wmi"
            }
            Err(e) => return Err(e),
        }
    };
    println!("launched via {how}; launcher: {}", process_report());
    Ok(())
}

/// Win32_Process.Create: the new process is a child of the WMI provider host, outside our job.
fn wmi_create(cmdline: &str) -> io::Result<()> {
    let script = format!(
        "$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine='{}'}}; exit $r.ReturnValue",
        cmdline.replace('\'', "''")
    );
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let status = process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &base64(&utf16)])
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!("Win32_Process.Create returned {status}")));
    }
    Ok(())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn heartbeat(path: &str, secs: u64) -> io::Result<()> {
    let header = process_report();
    for tick in 0..secs {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        std::fs::write(path, format!("{header}\ntick={tick} at={now}\n"))?;
        thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

// ---- job objects ----

fn job_run(secs: u64, cmdline: &str) -> io::Result<()> {
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    check(
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                mem::size_of_val(&limits) as u32,
            )
        },
        "SetInformationJobObject",
    )?;
    let pi = create_process(cmdline, CREATE_SUSPENDED | CREATE_NO_WINDOW)?;
    check(unsafe { AssignProcessToJobObject(job, pi.hProcess) }, "AssignProcessToJobObject")?;
    unsafe { ResumeThread(pi.hThread) };
    println!("started pid {}", pi.dwProcessId);
    for t in (0..secs).step_by(5) {
        thread::sleep(Duration::from_secs(5.min(secs - t)));
        let (active, total) = job_counts(job);
        println!("t={}s job processes: active={active} total={total}", t + 5.min(secs - t));
    }
    check(unsafe { TerminateJobObject(job, 1) }, "TerminateJobObject")?;
    thread::sleep(Duration::from_millis(500));
    let (active, total) = job_counts(job);
    println!("after terminate: active={active} total={total}");
    Ok(())
}

fn job_counts(job: HANDLE) -> (u32, u32) {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { mem::zeroed() };
    unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            mem::size_of_val(&info) as u32,
            null_mut(),
        )
    };
    (info.ActiveProcesses, info.TotalProcesses)
}

// ---- named pipes ----

fn dacl(variant: &str) -> String {
    let token = process_token();
    let user = user_sid(token);
    let logon = groups(token, TokenLogonSid).into_iter().next().unwrap_or_default();
    // P: protected, no inherited ACEs. GA: generic all.
    let extra = match variant {
        "user" => String::new(),
        "logon" => format!("(A;;GA;;;{logon})"),
        "restricted" => "(A;;GA;;;RC)".into(),
        "everyone" => "(A;;GA;;;WD)".into(),
        other => panic!("unknown dacl variant {other}"),
    };
    format!("D:P(A;;GA;;;{user}){extra}")
}

fn serve(name: &str, variant: &str, count: u32) -> io::Result<()> {
    let sddl = dacl(variant);
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    check(
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(&sddl).as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) },
        "ConvertStringSecurityDescriptorToSecurityDescriptorW",
    )?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let path = format!(r"\\.\pipe\{name}");
    println!("serving {path} with {sddl}");
    io::stdout().flush()?;
    for _ in 0..count {
        let pipe = unsafe {
            CreateNamedPipeW(
                wide(&path).as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                &sa,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        if unsafe { ConnectNamedPipe(pipe, null_mut()) } == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            return Err(io::Error::last_os_error());
        }
        let mut file = unsafe { File::from_raw_handle(pipe) };
        let mut buf = [0u8; 256];
        let n = file.read(&mut buf)?;
        let mut pid = 0;
        unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) };
        // Impersonation needs at least one read from the client first.
        let report = if unsafe { ImpersonateNamedPipeClient(pipe) } != 0 {
            let mut token = null_mut();
            unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
            let r = token_report(token);
            unsafe {
                CloseHandle(token);
                RevertToSelf();
            }
            r
        } else {
            format!("impersonation failed: {}", io::Error::last_os_error())
        };
        println!("client pid={pid} said {:?}; {report}", String::from_utf8_lossy(&buf[..n]).trim());
        io::stdout().flush()?;
        file.write_all(b"ok\n")?;
        file.flush()?;
        unsafe { DisconnectNamedPipe(pipe) };
    }
    unsafe { LocalFree(sd) };
    Ok(())
}

fn connect(name: &str) -> io::Result<()> {
    let mut pipe = OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\{name}"))?;
    pipe.write_all(b"hello\n")?;
    let mut buf = [0u8; 64];
    let n = pipe.read(&mut buf)?;
    println!("server replied {:?}; me: {}", String::from_utf8_lossy(&buf[..n]).trim(), process_report());
    Ok(())
}
