//! Starting a session process that does not belong to its caller.
//!
//! Emacs usually runs as a systemd user service, and stopping that service
//! kills every process in its cgroup.  So when the systemd user manager is
//! reachable, a session runs as its own transient service,
//! `ghostel-mux-<id>.service`.  The service has `Type=notify`, so
//! `systemd-run` returns once the session has bound its sockets.  Without
//! systemd, the session detaches by forking twice and reports readiness over
//! a pipe.

use std::io::{self, Read};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::process::Command;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Launcher {
    Systemd,
    Fork,
}

impl Launcher {
    /// The launcher named by `GHOSTEL_MUX_LAUNCHER` (`systemd` or `fork`), or
    /// systemd when the user manager's socket exists.
    pub fn choose() -> Launcher {
        match std::env::var("GHOSTEL_MUX_LAUNCHER").as_deref() {
            Ok("systemd") => Launcher::Systemd,
            Ok("fork") => Launcher::Fork,
            _ => {
                let user_manager = std::env::var_os("XDG_RUNTIME_DIR")
                    .map(|dir| std::path::Path::new(&dir).join("systemd/private").exists())
                    .unwrap_or(false);
                if user_manager {
                    Launcher::Systemd
                } else {
                    Launcher::Fork
                }
            }
        }
    }
}

pub fn start(id: &str, launcher: Launcher) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    match launcher {
        Launcher::Systemd => {
            let status = Command::new("systemd-run")
                .args(["--user", "--quiet", "--collect"])
                .arg(format!("--unit=ghostel-mux-{id}"))
                .args(["--property=Type=notify", "--property=NotifyAccess=main"])
                .arg("--")
                .arg(&exe)
                .args(["session", id])
                .status()?;
            if !status.success() {
                return Err(io::Error::other(format!("systemd-run failed: {status}")));
            }
            Ok(())
        }
        Launcher::Fork => fork_start(&exe, id),
    }
}

fn fork_start(exe: &std::path::Path, id: &str) -> io::Result<()> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    unsafe { libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC) };

    // Build everything the grandchild needs before forking.
    let program =
        std::ffi::CString::new(exe.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    let args = [
        program.clone(),
        std::ffi::CString::new("session").unwrap(),
        std::ffi::CString::new(id).map_err(io::Error::other)?,
        std::ffi::CString::new("--ready-fd").unwrap(),
        std::ffi::CString::new(write_fd.to_string()).unwrap(),
    ];
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let devnull = std::ffi::CString::new("/dev/null").unwrap();

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            libc::setsid();
            if libc::fork() != 0 {
                libc::_exit(0);
            }
            let null = libc::open(devnull.as_ptr(), libc::O_RDWR);
            for target in 0..3 {
                libc::dup2(null, target);
            }
            libc::execv(program.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }
    unsafe { libc::close(write_fd) };
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let mut ready = unsafe { std::fs::File::from(OwnedFd::from_raw_fd(read_fd)) };
    let mut byte = [0u8; 1];
    match ready.read(&mut byte) {
        Ok(1) => Ok(()),
        _ => Err(io::Error::other("the session exited before it was ready")),
    }
}

/// Tell whoever started this session that its sockets are bound.
pub fn notify_ready(ready_fd: Option<RawFd>) {
    if let Some(fd) = ready_fd {
        unsafe {
            libc::write(fd, b"R".as_ptr() as *const libc::c_void, 1);
            libc::close(fd);
        }
    }
    if let Some(path) = std::env::var_os("NOTIFY_SOCKET") {
        let Ok(socket) = UnixDatagram::unbound() else {
            return;
        };
        let bytes = path.as_encoded_bytes();
        if let Some(name) = bytes.strip_prefix(b"@") {
            use std::os::linux::net::SocketAddrExt;
            if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name) {
                let _ = socket.send_to_addr(b"READY=1", &addr);
            }
        } else {
            let _ = socket.send_to(b"READY=1", &path);
        }
    }
}
