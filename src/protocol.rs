//! Messages on a session's `ctl` socket.
//!
//! Each message is one line of JSON.  Requests carry an `op` field and
//! events a `type` field.  The one exception to line framing is a snapshot:
//! the `snapshot` event line gives a byte count, and that many raw bytes
//! follow it directly.
//!
//! The Emacs that owns a session keeps its `ctl` connection open for as long
//! as it shows the terminal; closing it detaches.  Other clients, such as
//! `ghostel-mux list`, connect, send requests, and disconnect.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    /// Report the session's metadata.
    Info,
    /// Send a snapshot and then the metadata, without changing the owner,
    /// so a client can check that it can restore the snapshot before it
    /// attaches.  Answers an error while the terminal is mid-sequence.
    Snapshot,
    /// Start the command.  Only valid once, before anything has attached;
    /// the requester becomes the owner without a snapshot.
    Spawn {
        client: String,
        argv: Vec<String>,
        env: Vec<String>,
        cwd: String,
        #[serde(flatten)]
        size: Size,
        scrollback: usize,
        #[serde(flatten)]
        kitty: Kitty,
        #[serde(default)]
        name: Option<String>,
    },
    /// Become the owner.  Without `steal`, an owned session answers `busy`.
    Attach {
        client: String,
        #[serde(default)]
        steal: bool,
    },
    Resize {
        #[serde(flatten)]
        size: Size,
    },
    /// Give up ownership; the session keeps running.
    Detach,
    /// Force the current owner off without attaching.
    Evict,
    /// Hang up the terminal and end the session once the command exits.
    Kill,
    SetName {
        name: Option<String>,
    },
    /// The owner has recorded the exit status; the data stream may close.
    ExitAck,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub cell_width: u32,
    #[serde(default)]
    pub cell_height: u32,
}

/// How the session's terminal keeps Kitty graphics images.  The owning
/// Emacs sends its settings, so images the program sends while no Emacs is attached
/// are kept the way that Emacs would keep them.
#[derive(Debug, Clone, Deserialize)]
pub struct Kitty {
    /// Bytes of image storage per screen; zero turns Kitty graphics off.
    #[serde(rename = "kitty_storage_limit", default = "default_kitty_storage_limit")]
    pub storage_limit: u64,
    /// Ways a program may hand over an image besides sending its bytes:
    /// bit 0 a file, bit 1 a temporary file, bit 2 shared memory.
    #[serde(rename = "kitty_mediums", default)]
    pub mediums: u32,
    /// The directory temporary-file images must be in.
    #[serde(rename = "kitty_temp_dir", default)]
    pub temp_dir: Option<String>,
}

fn default_kitty_storage_limit() -> u64 {
    320 * 1024 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    pub client: String,
    pub pid: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub name: Option<String>,
    pub argv: Vec<String>,
    pub cwd: String,
    /// The command's pid, once it has started.
    pub pid: Option<i32>,
    pub tty: Option<String>,
    pub title: String,
    pub pwd: String,
    pub rows: u16,
    pub cols: u16,
    pub owner: Option<Owner>,
    /// The client that owned the session before it was last detached.
    pub last_owner: Option<String>,
    /// Seconds since the epoch.
    pub created: u64,
    pub exit_status: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Event {
    Info {
        info: Info,
    },
    Ok,
    Error {
        message: String,
    },
    Busy {
        owner: Owner,
    },
    /// `bytes` raw snapshot bytes follow this line.
    Snapshot {
        bytes: usize,
    },
    /// The requester owns the session; it may connect to `data` now.
    Attached {
        info: Info,
    },
    /// Another client took the session.
    Stolen {
        by: String,
    },
    /// Another client forced this owner off.
    Evicted,
    Exit {
        status: i32,
    },
}

pub fn encode(event: &Event) -> Vec<u8> {
    let mut line = serde_json::to_vec(event).expect("events always serialize");
    line.push(b'\n');
    line
}
