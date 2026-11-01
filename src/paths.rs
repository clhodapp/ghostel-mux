//! Where sessions live.
//!
//! Each session is a directory under `$XDG_RUNTIME_DIR/ghostel-mux/` named by
//! its id, holding two sockets: `ctl` for requests and events, and `data` for
//! the terminal byte stream of the Emacs that owns the session.  Listing the
//! directory lists the sessions of every Emacs this user runs.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;

pub fn root() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/ghostel-mux-{}", unsafe { libc::getuid() }))
        });
    base.join("ghostel-mux")
}

/// Create the root directory, readable only by this user.
pub fn ensure_root() -> io::Result<PathBuf> {
    let root = root();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    Ok(root)
}

pub struct SessionPaths {
    pub dir: PathBuf,
    pub ctl: PathBuf,
    pub data: PathBuf,
}

impl SessionPaths {
    pub fn new(id: &str) -> SessionPaths {
        let dir = root().join(id);
        SessionPaths {
            ctl: dir.join("ctl"),
            data: dir.join("data"),
            dir,
        }
    }
}

/// A session id must be a single path component.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Ids of all session directories, in no particular order.
pub fn session_ids() -> io::Result<Vec<String>> {
    let mut ids = Vec::new();
    match fs::read_dir(root()) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if let Some(name) = entry.file_name().to_str() {
                    if valid_id(name) && entry.path().join("ctl").exists() {
                        ids.push(name.to_string());
                    }
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    Ok(ids)
}

/// A new random session id.
pub fn new_id() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    let n = unsafe { libc::getrandom(bytes.as_mut_ptr() as *mut libc::c_void, bytes.len(), 0) };
    if n != bytes.len() as isize {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
