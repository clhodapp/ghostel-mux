//! The session's own copy of the terminal state: a headless libghostty-vt
//! terminal fed with every byte the shell writes.
//!
//! A snapshot of it is what an attaching Emacs starts from.  While no Emacs
//! owns the session, this terminal also answers the shell's terminal queries
//! (device attributes, cursor position reports) so programs do not hang
//! waiting for a reply.

use std::ffi::c_void;
use std::io;
use std::os::fd::RawFd;
use std::ptr;

use crate::ffi;
use crate::protocol::Kitty;

/// DEC private mode 2027, grapheme clustering, which ghostel turns on by
/// default for its own terminals.
const MODE_GRAPHEME_CLUSTER: ffi::GhosttyMode = 2027;

/// Where terminal replies go.  Boxed so the callback's userdata pointer stays
/// valid when `Terminal` moves.
struct Replies {
    pty: RawFd,
    enabled: bool,
}

pub struct Terminal {
    raw: ffi::GhosttyTerminal,
    replies: Box<Replies>,
}

fn check(result: ffi::GhosttyResult, what: &str) -> io::Result<()> {
    if result == ffi::GHOSTTY_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{what} failed: libghostty result {result}"
        )))
    }
}

unsafe extern "C" fn write_pty(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    let replies = &*(userdata as *const Replies);
    if !replies.enabled || replies.pty < 0 {
        return;
    }
    // Best effort, like ghostel: a reply that does not fit in the PTY's
    // input queue is dropped rather than blocking the session.
    libc::write(replies.pty, data as *const c_void, len);
}

impl Terminal {
    pub fn new(cols: u16, rows: u16, scrollback_bytes: usize) -> io::Result<Terminal> {
        let mut raw: ffi::GhosttyTerminal = ptr::null_mut();
        check(
            unsafe { ffi::ghostty_terminal_new(ptr::null(), &mut raw, cols.max(1), rows.max(1)) },
            "ghostty_terminal_new",
        )?;
        let mut terminal = Terminal {
            raw,
            replies: Box::new(Replies {
                pty: -1,
                enabled: true,
            }),
        };
        let scrollback = scrollback_bytes;
        terminal.set(
            ffi::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
            &scrollback as *const usize as *const c_void,
            "scrollback limit",
        )?;
        let mode = ffi::GhosttyTerminalModeConfig {
            mode: MODE_GRAPHEME_CLUSTER,
            value: true,
        };
        terminal.set(
            ffi::GHOSTTY_TERMINAL_OPT_MODE_DEFAULT,
            &mode as *const _ as *const c_void,
            "grapheme cluster mode",
        )?;
        let userdata = &*terminal.replies as *const Replies as *const c_void;
        terminal.set(ffi::GHOSTTY_TERMINAL_OPT_USERDATA, userdata, "userdata")?;
        let callback: ffi::GhosttyTerminalWritePtyFn = Some(write_pty);
        terminal.set(
            ffi::GHOSTTY_TERMINAL_OPT_WRITE_PTY,
            callback.map_or(ptr::null(), |f| f as *const c_void),
            "write_pty callback",
        )?;
        Ok(terminal)
    }

    fn set(
        &mut self,
        option: ffi::GhosttyTerminalOption,
        value: *const c_void,
        what: &str,
    ) -> io::Result<()> {
        check(
            unsafe { ffi::ghostty_terminal_set(self.raw, option, value) },
            what,
        )
    }

    /// Keep Kitty graphics images as KITTY says.
    pub fn set_kitty(&mut self, kitty: &Kitty) -> io::Result<()> {
        self.set(
            ffi::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
            &kitty.storage_limit as *const u64 as *const c_void,
            "kitty storage limit",
        )?;
        let file = kitty.mediums & 1 != 0;
        self.set(
            ffi::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE,
            &file as *const bool as *const c_void,
            "kitty file medium",
        )?;
        let shared = kitty.mediums & 4 != 0;
        self.set(
            ffi::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_SHARED_MEM,
            &shared as *const bool as *const c_void,
            "kitty shared memory medium",
        )?;
        match (&kitty.temp_dir, kitty.mediums & 2 != 0) {
            (Some(dir), true) => {
                let dir = ffi::GhosttyString {
                    ptr: dir.as_ptr(),
                    len: dir.len(),
                };
                self.set(
                    ffi::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_TEMP_FILE,
                    &dir as *const _ as *const c_void,
                    "kitty temporary file medium",
                )
            }
            _ => self.set(
                ffi::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_TEMP_FILE,
                ptr::null(),
                "kitty temporary file medium",
            ),
        }
    }

    /// Send replies to terminal queries to PTY, the session's PTY primary.
    pub fn set_reply_pty(&mut self, pty: RawFd) {
        self.replies.pty = pty;
    }

    /// Whether this terminal answers queries.  Off while an Emacs owns the
    /// session, because that Emacs's terminal answers them.
    pub fn set_replies_enabled(&mut self, enabled: bool) {
        self.replies.enabled = enabled;
    }

    pub fn write(&mut self, data: &[u8]) {
        if !data.is_empty() {
            unsafe { ffi::ghostty_terminal_vt_write(self.raw, data.as_ptr(), data.len()) }
        }
    }

    /// Write the shortest prefix of DATA that brings the parser to ground.
    /// Returns the length of that prefix, or None when all of DATA was
    /// written and the parser is still in the middle of a sequence.
    pub fn write_until_ground(&mut self, data: &[u8]) -> Option<usize> {
        let mut stop: ffi::GhosttyTerminalWriteStop = unsafe { std::mem::zeroed() };
        stop.size = std::mem::size_of::<ffi::GhosttyTerminalWriteStop>();
        let result = unsafe {
            ffi::ghostty_terminal_vt_write_until(
                self.raw,
                data.as_ptr(),
                data.len(),
                ffi::GHOSTTY_TERMINAL_BOUNDARY_GROUND as u32,
                &mut stop,
            )
        };
        (result == ffi::GHOSTTY_SUCCESS).then_some(stop.consumed)
    }

    /// Whether the parser is between sequences, where a snapshot can be cut.
    pub fn at_ground(&self) -> bool {
        let mut ground = false;
        let result = unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::GHOSTTY_TERMINAL_DATA_VT_GROUND,
                &mut ground as *mut bool as *mut c_void,
            )
        };
        result == ffi::GHOSTTY_SUCCESS && ground
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
    ) -> io::Result<()> {
        check(
            unsafe {
                ffi::ghostty_terminal_resize(
                    self.raw,
                    cols.max(1),
                    rows.max(1),
                    cell_width,
                    cell_height,
                )
            },
            "ghostty_terminal_resize",
        )
    }

    /// Encode the complete terminal state.  The parser must be at ground.
    pub fn snapshot(&self) -> io::Result<Vec<u8>> {
        let mut ptr: *mut u8 = ptr::null_mut();
        let mut len = 0usize;
        check(
            unsafe {
                ffi::ghostty_snapshot_encode_alloc(self.raw, ptr::null(), &mut ptr, &mut len)
            },
            "ghostty_snapshot_encode_alloc",
        )?;
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        unsafe { ffi::ghostty_free(ptr::null(), ptr, len) };
        Ok(bytes)
    }

    fn string(&self, data: ffi::GhosttyTerminalData) -> String {
        let mut value = ffi::GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        let result = unsafe {
            ffi::ghostty_terminal_get(self.raw, data, &mut value as *mut _ as *mut c_void)
        };
        if result != ffi::GHOSTTY_SUCCESS || value.ptr.is_null() || value.len == 0 {
            return String::new();
        }
        let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    pub fn title(&self) -> String {
        self.string(ffi::GHOSTTY_TERMINAL_DATA_TITLE)
    }

    pub fn pwd(&self) -> String {
        self.string(ffi::GHOSTTY_TERMINAL_DATA_PWD)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_terminal_free(self.raw) }
    }
}
