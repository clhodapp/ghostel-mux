//! Starting the shell on a new PTY.
//!
//! The PTY setup matches ghostel's native backend, so a program sees the same
//! terminal whether ghostel or a session started it: the child gets its own
//! session with the PTY as controlling terminal, UTF-8 input mode on, and
//! XON/XOFF flow control off so C-q and C-s reach the program.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;

pub struct Spawned {
    pub primary: OwnedFd,
    pub pid: libc::pid_t,
    pub tty: String,
}

pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
    pub width_px: u16,
    pub height_px: u16,
}

fn last_error(what: &str) -> io::Error {
    let err = io::Error::last_os_error();
    io::Error::new(err.kind(), format!("{what}: {err}"))
}

fn cstrings(items: &[String]) -> io::Result<Vec<CString>> {
    items
        .iter()
        .map(|s| CString::new(s.as_bytes()).map_err(|_| io::Error::other("NUL byte in argument")))
        .collect()
}

pub fn set_winsize(fd: RawFd, size: &WinSize) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.width_px,
        ws_ypixel: size.height_px,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } != 0 {
        return Err(last_error("TIOCSWINSZ"));
    }
    Ok(())
}

/// The foreground process group of the PTY, read on the primary.
pub fn foreground_pgid(fd: RawFd) -> Option<libc::pid_t> {
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    (pgid > 0).then_some(pgid)
}

pub fn spawn(argv: &[String], env: &[String], cwd: &str, size: &WinSize) -> io::Result<Spawned> {
    if argv.is_empty() {
        return Err(io::Error::other("empty command"));
    }
    let c_argv = cstrings(argv)?;
    let c_env = cstrings(env)?;
    let c_cwd = CString::new(cwd).map_err(|_| io::Error::other("NUL byte in directory"))?;
    let mut argv_ptrs: Vec<*const libc::c_char> = c_argv.iter().map(|s| s.as_ptr()).collect();
    argv_ptrs.push(ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = c_env.iter().map(|s| s.as_ptr()).collect();
    env_ptrs.push(ptr::null());

    let primary = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if primary < 0 {
        return Err(last_error("posix_openpt"));
    }
    let primary = unsafe { OwnedFd::from_raw_fd(primary) };
    let pfd = primary.as_raw_fd();
    if unsafe { libc::grantpt(pfd) } != 0 {
        return Err(last_error("grantpt"));
    }
    if unsafe { libc::unlockpt(pfd) } != 0 {
        return Err(last_error("unlockpt"));
    }
    let mut name = [0 as libc::c_char; 256];
    if unsafe { libc::ptsname_r(pfd, name.as_mut_ptr(), name.len()) } != 0 {
        return Err(last_error("ptsname_r"));
    }
    let tty = unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    let replica = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if replica < 0 {
        return Err(last_error("open replica"));
    }
    let replica = unsafe { OwnedFd::from_raw_fd(replica) };
    let rfd = replica.as_raw_fd();

    let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(rfd, &mut attrs) } != 0 {
        return Err(last_error("tcgetattr"));
    }
    attrs.c_iflag |= libc::IUTF8;
    attrs.c_iflag &= !libc::IXON;
    if unsafe { libc::tcsetattr(rfd, libc::TCSANOW, &attrs) } != 0 {
        return Err(last_error("tcsetattr"));
    }
    set_winsize(pfd, size)?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(last_error("fork"));
    }
    if pid == 0 {
        // The child: only async-signal-safe calls until execve.
        unsafe {
            let fail = |msg: &[u8]| -> ! {
                libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
                libc::_exit(127);
            };
            let mut unblocked: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut unblocked);
            libc::sigprocmask(libc::SIG_SETMASK, &unblocked, ptr::null_mut());
            for sig in [
                libc::SIGPIPE,
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGTERM,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            if libc::setsid() < 0 {
                fail(b"ghostel-mux: setsid failed\n");
            }
            if libc::ioctl(rfd, libc::TIOCSCTTY, 0) != 0 {
                fail(b"ghostel-mux: TIOCSCTTY failed\n");
            }
            for target in 0..3 {
                if libc::dup2(rfd, target) < 0 {
                    fail(b"ghostel-mux: dup2 failed\n");
                }
            }
            if libc::chdir(c_cwd.as_ptr()) != 0 {
                fail(b"ghostel-mux: cannot change to the working directory\n");
            }
            libc::execve(argv_ptrs[0], argv_ptrs.as_ptr(), env_ptrs.as_ptr());
            fail(b"ghostel-mux: cannot run the command\n");
        }
    }
    drop(replica);
    Ok(Spawned { primary, pid, tty })
}
