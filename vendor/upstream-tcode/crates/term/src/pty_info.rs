use std::{
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::fs::File;

#[cfg(unix)]
use std::os::fd::AsRawFd as _;

use crate::sync::MutexExt as _;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub name: String,
    pub cwd: PathBuf,
}

pub(crate) struct PtyInfo {
    #[cfg(unix)]
    file: File,
    #[cfg(unix)]
    fallback_pid: u32,
    last_refresh: Mutex<Option<Instant>>,
}

impl PtyInfo {
    #[cfg(unix)]
    pub fn new(file: File, fallback_pid: u32) -> Self {
        Self {
            file,
            fallback_pid,
            last_refresh: Mutex::new(None),
        }
    }

    #[cfg(not(unix))]
    pub fn new() -> Self {
        Self {
            last_refresh: Mutex::new(None),
        }
    }

    pub fn should_refresh(&self) -> bool {
        let mut last = self.last_refresh.lock_recover();
        if last.is_some_and(|time| time.elapsed() < Duration::from_millis(250)) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    #[cfg(unix)]
    pub fn load(&self) -> Option<ProcessInfo> {
        let foreground = unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) };
        let pid = if foreground > 0 {
            foreground as u32
        } else {
            self.fallback_pid
        };
        if pid == 0 {
            return None;
        }
        load_process(pid)
    }

    #[cfg(not(unix))]
    pub fn load(&self) -> Option<ProcessInfo> {
        None
    }
}

#[cfg(target_os = "linux")]
fn load_process(pid: u32) -> Option<ProcessInfo> {
    let proc = PathBuf::from("/proc").join(pid.to_string());
    let name = std::fs::read_to_string(proc.join("comm"))
        .ok()?
        .trim_end_matches(['\r', '\n'])
        .to_string();
    let cwd = std::fs::read_link(proc.join("cwd")).ok()?;
    Some(ProcessInfo { name, cwd })
}

#[cfg(target_os = "macos")]
fn load_process(pid: u32) -> Option<ProcessInfo> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt as _, path::Path};

    let mut executable = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `executable` is writable for the reported capacity and remains
    // alive for the duration of the call.
    let executable_len = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            executable.as_mut_ptr().cast(),
            executable.len() as u32,
        )
    };
    if executable_len <= 0 {
        return None;
    }
    executable.truncate(executable_len as usize);
    let name = Path::new(std::ffi::OsStr::from_bytes(&executable))
        .file_name()?
        .to_string_lossy()
        .into_owned();

    // SAFETY: `proc_vnodepathinfo` is a C data structure for which an all-zero
    // bit pattern is valid, and zeroing guarantees bounded C strings even if
    // the kernel writes a partial result.
    let mut paths = unsafe { std::mem::zeroed::<libc::proc_vnodepathinfo>() };
    // SAFETY: `paths` points to suitably aligned, writable storage of exactly
    // the size passed to `proc_pidinfo`.
    let paths_len = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut paths as *mut libc::proc_vnodepathinfo).cast(),
            std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int,
        )
    };
    if paths_len <= 0 || paths.pvi_cdir.vip_vi.vi_stat.vst_dev == 0 {
        return None;
    }
    // SAFETY: `vip_path` is a fixed-size, NUL-terminated C path buffer filled
    // by the kernel on a successful `proc_pidinfo` call.
    let cwd = unsafe { CStr::from_ptr(paths.pvi_cdir.vip_path.as_ptr().cast()) };
    let cwd = PathBuf::from(std::ffi::OsStr::from_bytes(cwd.to_bytes()));
    Some(ProcessInfo { name, cwd })
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn load_process(_pid: u32) -> Option<ProcessInfo> {
    None
}

impl PtyInfo {
    /// Whether the shell is in the foreground with no child process, by
    /// inspecting the process tree now. `None` when it cannot be told.
    #[cfg(unix)]
    pub fn idle_shell(&self, shell_name: &str) -> Option<bool> {
        let foreground = unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) };
        if foreground <= 0 || self.fallback_pid == 0 {
            return None;
        }
        if foreground as u32 != self.fallback_pid {
            return Some(false);
        }
        let process = load_process(self.fallback_pid)?;
        if process.name != shell_name {
            return Some(false);
        }
        has_children(self.fallback_pid).map(|children| !children)
    }

    #[cfg(not(unix))]
    pub fn idle_shell(&self, _shell_name: &str) -> Option<bool> {
        None
    }
}

#[cfg(target_os = "linux")]
fn has_children(pid: u32) -> Option<bool> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    std::fs::read_to_string(path)
        .ok()
        .map(|children| !children.trim().is_empty())
}

#[cfg(target_os = "macos")]
fn has_children(pid: u32) -> Option<bool> {
    // libproc's PROC_PPID_ONLY selector is 6. A one-pid buffer suffices: any child blocks closure.
    let mut child: libc::pid_t = 0;
    let bytes = unsafe {
        *libc::__error() = 0;
        libc::proc_listpids(
            6,
            pid,
            (&mut child as *mut libc::pid_t).cast(),
            std::mem::size_of_val(&child) as libc::c_int,
        )
    };
    if bytes < 0
        || (bytes == 0
            && std::io::Error::last_os_error()
                .raw_os_error()
                .is_some_and(|error| error != 0))
    {
        None
    } else {
        Some(bytes > 0 && child > 0)
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn has_children(_pid: u32) -> Option<bool> {
    None
}
