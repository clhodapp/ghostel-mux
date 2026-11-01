//! One terminal session: a command on a PTY, the session's own terminal
//! state, and at most one owning Emacs.
//!
//! The session is a single-threaded `poll(2)` loop.  Output from the PTY goes
//! first to the owner's data stream and then into the session's terminal, so
//! keeping the copy adds no latency to what the owner sees.  When the owner
//! reads too slowly, the session stops reading the PTY, which slows the
//! program down the way a real terminal would.  With no owner, output only
//! updates the session's terminal.
//!
//! Attaching cuts a snapshot of the session's terminal at an exact point in
//! the byte stream; every later byte goes to the new owner's data stream.
//! The cut has to fall where the VT parser is between sequences, so when an
//! attach arrives mid-sequence the session first feeds the PTY bytes that
//! finish the sequence into its terminal.

use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::paths::SessionPaths;
use crate::protocol::{self, Event, Info, Kitty, Owner, Request, Size};
use crate::pty::{self, WinSize};
use crate::terminal::Terminal;

const READ_CHUNK: usize = 64 * 1024;
/// Bytes queued for the owner beyond which the session stops reading the PTY.
const HIGH_WATER: usize = 4 << 20;
/// How long a new session waits for its command before giving up.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after the command exits the session keeps reading output from
/// processes that still hold the PTY open.
const EOF_GRACE: Duration = Duration::from_secs(2);
/// How long an attach waits for the parser to reach ground before cancelling
/// the unfinished sequence in the session's terminal.
const GROUND_TIMEOUT: Duration = Duration::from_secs(1);
/// How long after a hang-up the command may run before it is killed.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// CAN, which cancels any escape sequence in progress.
const CANCEL: &[u8] = b"\x18";

struct Client {
    id: u64,
    stream: UnixStream,
    pid: i32,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    close_after_flush: bool,
    dead: bool,
}

impl Client {
    fn queue(&mut self, bytes: &[u8]) {
        self.wbuf.extend_from_slice(bytes);
        self.flush();
    }

    fn send(&mut self, event: &Event) {
        self.queue(&protocol::encode(event));
    }

    fn flush(&mut self) {
        flush_into(&mut self.stream, &mut self.wbuf, &mut self.dead);
        if self.close_after_flush && self.wbuf.is_empty() {
            self.dead = true;
        }
    }
}

struct DataConn {
    stream: UnixStream,
    wbuf: Vec<u8>,
    dead: bool,
}

fn flush_into(stream: &mut UnixStream, wbuf: &mut Vec<u8>, dead: &mut bool) {
    while !wbuf.is_empty() && !*dead {
        match stream.write(wbuf) {
            Ok(0) => *dead = true,
            Ok(n) => {
                wbuf.drain(..n);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => *dead = true,
        }
    }
}

struct Child {
    pid: libc::pid_t,
    pidfd: OwnedFd,
    primary: OwnedFd,
    tty: String,
}

pub struct Session {
    id: String,
    paths: SessionPaths,
    created: u64,
    started: Instant,
    ctl: UnixListener,
    data_listener: UnixListener,
    clients: Vec<Client>,
    next_client: u64,

    owner: Option<u64>,
    owner_label: Option<String>,
    last_owner: Option<String>,
    /// The pid allowed to connect to `data`, set once an attach succeeds.
    data_peer_pid: Option<i32>,
    data: Option<DataConn>,
    /// Output for the owner that arrived before it connected to `data`.
    pending: Vec<u8>,
    /// Set while an attach waits for the parser to reach ground.
    awaiting_ground: Option<Instant>,

    term: Option<Terminal>,
    child: Option<Child>,
    to_pty: Vec<u8>,
    pty_eof: bool,
    exited_at: Option<Instant>,
    exit_status: Option<i32>,
    exit_sent: bool,
    exit_acked: bool,
    killed_at: Option<Instant>,

    argv: Vec<String>,
    cwd: String,
    name: Option<String>,
    size: Size,
}

fn peer_pid(stream: &UnixStream) -> i32 {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 {
        cred.pid
    } else {
        -1
    }
}

fn pidfd_open(pid: libc::pid_t) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn bind(path: &std::path::Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// What the poll loop is waiting on, so its results can be matched back.
enum Slot {
    Ctl,
    DataListener,
    Client(usize),
    Data,
    Pty,
    PidFd,
}

impl Session {
    pub fn bind(id: &str, paths: SessionPaths) -> io::Result<Session> {
        std::fs::create_dir(&paths.dir)?;
        let ctl = bind(&paths.ctl)?;
        let data_listener = bind(&paths.data)?;
        Ok(Session {
            id: id.to_string(),
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            started: Instant::now(),
            paths,
            ctl,
            data_listener,
            clients: Vec::new(),
            next_client: 0,
            owner: None,
            owner_label: None,
            last_owner: None,
            data_peer_pid: None,
            data: None,
            pending: Vec::new(),
            awaiting_ground: None,
            term: None,
            child: None,
            to_pty: Vec::new(),
            pty_eof: false,
            exited_at: None,
            exit_status: None,
            exit_sent: false,
            exit_acked: false,
            killed_at: None,
            argv: Vec::new(),
            cwd: String::new(),
            name: None,
            size: Size {
                rows: 24,
                cols: 80,
                cell_width: 0,
                cell_height: 0,
            },
        })
    }

    fn info(&self) -> Info {
        let owner = self.owner.and_then(|id| {
            let client = self.clients.iter().find(|c| c.id == id)?;
            Some(Owner {
                client: self.owner_label.clone().unwrap_or_default(),
                pid: client.pid,
            })
        });
        Info {
            id: self.id.clone(),
            name: self.name.clone(),
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            pid: self.child.as_ref().map(|c| c.pid),
            tty: self.child.as_ref().map(|c| c.tty.clone()),
            title: self.term.as_ref().map(|t| t.title()).unwrap_or_default(),
            pwd: self.term.as_ref().map(|t| t.pwd()).unwrap_or_default(),
            rows: self.size.rows,
            cols: self.size.cols,
            owner,
            last_owner: self.last_owner.clone(),
            created: self.created,
            exit_status: self.exit_status,
        }
    }

    fn client(&mut self, id: u64) -> Option<&mut Client> {
        self.clients.iter_mut().find(|c| c.id == id && !c.dead)
    }

    fn reply(&mut self, id: u64, event: &Event) {
        if let Some(client) = self.client(id) {
            client.send(event);
        }
    }

    fn owner_backlog(&self) -> usize {
        self.pending.len() + self.data.as_ref().map_or(0, |d| d.wbuf.len())
    }

    // ----- Output from the PTY -----

    fn forward(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.owner.is_none() || self.awaiting_ground.is_some() {
            return;
        }
        if let Some(data) = self.data.as_mut() {
            data.wbuf.extend_from_slice(bytes);
            flush_into(&mut data.stream, &mut data.wbuf, &mut data.dead);
        } else if self.data_peer_pid.is_some() {
            self.pending.extend_from_slice(bytes);
        }
    }

    fn on_output(&mut self, chunk: &[u8]) {
        if self.awaiting_ground.is_some() {
            let reached = self.term.as_mut().and_then(|t| t.write_until_ground(chunk));
            if let Some(n) = reached {
                self.complete_attach();
                let rest = &chunk[n..];
                self.forward(rest);
                if let Some(term) = self.term.as_mut() {
                    term.write(rest);
                }
            }
            return;
        }
        self.forward(chunk);
        if let Some(term) = self.term.as_mut() {
            term.write(chunk);
        }
    }

    fn read_pty(&mut self) {
        let Some(child) = self.child.as_ref() else {
            return;
        };
        let fd = child.primary.as_raw_fd();
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n > 0 {
                self.on_output(&buf[..n as usize]);
                if self.owner_backlog() > HIGH_WATER {
                    return;
                }
                continue;
            }
            if n == 0 {
                self.pty_eof = true;
                return;
            }
            match io::Error::last_os_error().kind() {
                ErrorKind::WouldBlock => return,
                ErrorKind::Interrupted => continue,
                // EIO: every process holding the replica has closed it.
                _ => {
                    self.pty_eof = true;
                    return;
                }
            }
        }
    }

    fn write_pty(&mut self) {
        let Some(child) = self.child.as_ref() else {
            self.to_pty.clear();
            return;
        };
        let fd = child.primary.as_raw_fd();
        while !self.to_pty.is_empty() {
            let n = unsafe {
                libc::write(
                    fd,
                    self.to_pty.as_ptr() as *const libc::c_void,
                    self.to_pty.len(),
                )
            };
            if n > 0 {
                self.to_pty.drain(..n as usize);
                continue;
            }
            match io::Error::last_os_error().kind() {
                ErrorKind::WouldBlock => return,
                ErrorKind::Interrupted => continue,
                _ => {
                    self.to_pty.clear();
                    return;
                }
            }
        }
    }

    // ----- Ownership -----

    /// Send the snapshot and let the owner connect to `data`.
    fn complete_attach(&mut self) {
        self.awaiting_ground = None;
        let Some(owner) = self.owner else { return };
        let snapshot = match self.term.as_ref().map(|t| t.snapshot()) {
            Some(Ok(bytes)) => bytes,
            Some(Err(err)) => {
                self.reply(
                    owner,
                    &Event::Error {
                        message: err.to_string(),
                    },
                );
                self.release_owner(None);
                return;
            }
            None => Vec::new(),
        };
        let info = self.info();
        let pid = self
            .clients
            .iter()
            .find(|c| c.id == owner)
            .map_or(-1, |c| c.pid);
        if let Some(client) = self.client(owner) {
            client.send(&Event::Snapshot {
                bytes: snapshot.len(),
            });
            client.queue(&snapshot);
            client.send(&Event::Attached { info });
        }
        self.data_peer_pid = Some(pid);
        self.pending.clear();
    }

    /// Drop the current owner.  NOTICE, when set, is the event that tells the
    /// owner why; its connection closes once that is written.
    fn release_owner(&mut self, notice: Option<Event>) {
        let Some(owner) = self.owner.take() else {
            return;
        };
        if let Some(notice) = notice {
            if let Some(client) = self.client(owner) {
                client.send(&notice);
                client.close_after_flush = true;
                client.flush();
            }
        }
        self.last_owner = self.owner_label.take();
        self.data = None;
        self.data_peer_pid = None;
        self.pending.clear();
        self.awaiting_ground = None;
        self.exit_sent = false;
        self.exit_acked = false;
        if let Some(term) = self.term.as_mut() {
            term.set_replies_enabled(true);
        }
    }

    fn take_ownership(&mut self, id: u64, label: String) {
        self.owner = Some(id);
        self.owner_label = Some(label);
        if let Some(term) = self.term.as_mut() {
            term.set_replies_enabled(false);
        }
    }

    fn attach(&mut self, id: u64, label: String, steal: bool) {
        if self.term.is_none() {
            self.reply(
                id,
                &Event::Error {
                    message: "the session has not started".into(),
                },
            );
            return;
        }
        if let Some(owner) = self.owner {
            if owner == id {
                self.reply(
                    id,
                    &Event::Error {
                        message: "already attached".into(),
                    },
                );
                return;
            }
            if !steal {
                let owner = self.info().owner.expect("an owner has a client");
                self.reply(id, &Event::Busy { owner });
                return;
            }
            self.release_owner(Some(Event::Stolen { by: label.clone() }));
        }
        self.take_ownership(id, label);
        let at_ground = self.term.as_ref().is_some_and(|t| t.at_ground());
        if at_ground {
            self.complete_attach();
        } else if self.pty_eof {
            // No byte will ever finish the sequence.
            if let Some(term) = self.term.as_mut() {
                term.write(CANCEL);
            }
            self.complete_attach();
        } else {
            self.awaiting_ground = Some(Instant::now());
        }
    }

    fn spawn(&mut self, id: u64, label: String, request: SpawnRequest) {
        if self.term.is_some() {
            self.reply(
                id,
                &Event::Error {
                    message: "the session has already started".into(),
                },
            );
            return;
        }
        let size = request.size;
        let mut term = match Terminal::new(size.cols, size.rows, request.scrollback)
            .and_then(|mut term| term.set_kitty(&request.kitty).map(|()| term))
        {
            Ok(term) => term,
            Err(err) => {
                self.reply(
                    id,
                    &Event::Error {
                        message: err.to_string(),
                    },
                );
                return;
            }
        };
        let _ = term.resize(size.cols, size.rows, size.cell_width, size.cell_height);
        let spawned = match pty::spawn(&request.argv, &request.env, &request.cwd, &winsize(&size)) {
            Ok(spawned) => spawned,
            Err(err) => {
                self.reply(
                    id,
                    &Event::Error {
                        message: err.to_string(),
                    },
                );
                return;
            }
        };
        set_nonblocking(spawned.primary.as_raw_fd());
        term.set_reply_pty(spawned.primary.as_raw_fd());
        let pidfd = match pidfd_open(spawned.pid) {
            Ok(fd) => fd,
            Err(err) => {
                unsafe { libc::kill(spawned.pid, libc::SIGKILL) };
                self.reply(
                    id,
                    &Event::Error {
                        message: format!("pidfd_open: {err}"),
                    },
                );
                return;
            }
        };
        self.term = Some(term);
        self.child = Some(Child {
            pid: spawned.pid,
            pidfd,
            primary: spawned.primary,
            tty: spawned.tty,
        });
        self.argv = request.argv;
        self.cwd = request.cwd;
        self.name = request.name;
        self.size = size;
        self.take_ownership(id, label);
        let pid = self
            .clients
            .iter()
            .find(|c| c.id == id)
            .map_or(-1, |c| c.pid);
        self.data_peer_pid = Some(pid);
        let info = self.info();
        self.reply(id, &Event::Attached { info });
    }

    /// Send client ID a snapshot and the metadata, leaving the owner as is.
    fn peek(&mut self, id: u64) {
        let snapshot = match self.term.as_ref() {
            None => Err("the session has not started".to_string()),
            Some(term) if !term.at_ground() => {
                Err("the terminal is in the middle of a sequence; try again".to_string())
            }
            Some(term) => term.snapshot().map_err(|err| err.to_string()),
        };
        match snapshot {
            Ok(bytes) => {
                let info = self.info();
                if let Some(client) = self.client(id) {
                    client.send(&Event::Snapshot { bytes: bytes.len() });
                    client.queue(&bytes);
                    client.send(&Event::Info { info });
                }
            }
            Err(message) => self.reply(id, &Event::Error { message }),
        }
    }

    fn resize(&mut self, size: Size) {
        self.size = size;
        if let Some(term) = self.term.as_mut() {
            let _ = term.resize(size.cols, size.rows, size.cell_width, size.cell_height);
        }
        if let Some(child) = self.child.as_ref() {
            let _ = pty::set_winsize(child.primary.as_raw_fd(), &winsize(&size));
        }
    }

    fn hang_up(&mut self) {
        if self.killed_at.is_none() {
            self.killed_at = Some(Instant::now());
        }
        if self.exit_status.is_some() {
            return;
        }
        if let Some(child) = self.child.as_ref() {
            if let Some(pgid) = pty::foreground_pgid(child.primary.as_raw_fd()) {
                unsafe { libc::kill(-pgid, libc::SIGHUP) };
            }
            unsafe { libc::kill(-child.pid, libc::SIGHUP) };
            unsafe { libc::kill(-child.pid, libc::SIGCONT) };
        }
    }

    // ----- Requests -----

    fn handle(&mut self, id: u64, line: &[u8]) {
        let request: Request = match serde_json::from_slice(line) {
            Ok(request) => request,
            Err(err) => {
                self.reply(
                    id,
                    &Event::Error {
                        message: format!("bad request: {err}"),
                    },
                );
                return;
            }
        };
        let is_owner = self.owner == Some(id);
        match request {
            Request::Info => {
                let info = self.info();
                self.reply(id, &Event::Info { info });
            }
            Request::Snapshot => self.peek(id),
            Request::Spawn {
                client,
                argv,
                env,
                cwd,
                size,
                scrollback,
                kitty,
                name,
            } => {
                self.spawn(
                    id,
                    client,
                    SpawnRequest {
                        argv,
                        env,
                        cwd,
                        size,
                        scrollback,
                        kitty,
                        name,
                    },
                );
            }
            Request::Attach { client, steal } => self.attach(id, client, steal),
            Request::Resize { size } if is_owner => self.resize(size),
            Request::Detach if is_owner => {
                self.release_owner(None);
                self.reply(id, &Event::Ok);
            }
            Request::Evict => {
                if self.owner.is_some() && !is_owner {
                    self.release_owner(Some(Event::Evicted));
                }
                self.reply(id, &Event::Ok);
            }
            Request::Kill => {
                self.hang_up();
                self.reply(id, &Event::Ok);
            }
            Request::SetName { name } => {
                self.name = name;
                self.reply(id, &Event::Ok);
            }
            Request::ExitAck if is_owner => self.exit_acked = true,
            Request::Resize { .. } | Request::Detach | Request::ExitAck => {
                self.reply(
                    id,
                    &Event::Error {
                        message: "only the owner may do that".into(),
                    },
                );
            }
        }
    }

    fn read_client(&mut self, index: usize) {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let client = &mut self.clients[index];
            match client.stream.read(&mut buf) {
                Ok(0) => {
                    client.dead = true;
                    break;
                }
                Ok(n) => client.rbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    client.dead = true;
                    break;
                }
            }
        }
        let id = self.clients[index].id;
        while let Some(pos) = self.clients[index].rbuf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.clients[index].rbuf.drain(..=pos).collect();
            self.handle(id, &line[..line.len() - 1]);
        }
    }

    fn accept_clients(&mut self) {
        while let Ok((stream, _)) = self.ctl.accept() {
            if stream.set_nonblocking(true).is_err() {
                continue;
            }
            let pid = peer_pid(&stream);
            self.next_client += 1;
            self.clients.push(Client {
                id: self.next_client,
                stream,
                pid,
                rbuf: Vec::new(),
                wbuf: Vec::new(),
                close_after_flush: false,
                dead: false,
            });
        }
    }

    fn accept_data(&mut self) {
        while let Ok((stream, _)) = self.data_listener.accept() {
            // Only the owner's process may take the data stream, and only
            // once per attach.
            let Some(expected) = self.data_peer_pid else {
                continue;
            };
            if self.owner.is_none()
                || peer_pid(&stream) != expected
                || stream.set_nonblocking(true).is_err()
            {
                continue;
            }
            self.data_peer_pid = None;
            let mut conn = DataConn {
                stream,
                wbuf: std::mem::take(&mut self.pending),
                dead: false,
            };
            flush_into(&mut conn.stream, &mut conn.wbuf, &mut conn.dead);
            self.data = Some(conn);
        }
    }

    fn read_data(&mut self) {
        let mut buf = [0u8; 16 * 1024];
        let Some(data) = self.data.as_mut() else {
            return;
        };
        loop {
            match data.stream.read(&mut buf) {
                Ok(0) => {
                    // The owner stopped its reader; output has nowhere to go.
                    data.dead = true;
                    break;
                }
                Ok(n) => self.to_pty.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    data.dead = true;
                    break;
                }
            }
        }
        self.write_pty();
    }

    fn reap(&mut self) {
        let Some(child) = self.child.as_ref() else {
            return;
        };
        let mut status = 0;
        let rc = unsafe { libc::waitpid(child.pid, &mut status, libc::WNOHANG) };
        if rc != child.pid {
            return;
        }
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            return;
        };
        self.exit_status = Some(code);
        self.exited_at = Some(Instant::now());
    }

    // ----- Lifecycle -----

    /// Advance the time- and state-driven parts of the lifecycle.  Returns
    /// false when the session is finished.
    fn tick(&mut self) -> bool {
        let now = Instant::now();
        if self.child.is_none()
            && (self.killed_at.is_some() || now.duration_since(self.started) > SPAWN_TIMEOUT)
        {
            return false;
        }
        if let Some(since) = self.awaiting_ground {
            if now.duration_since(since) > GROUND_TIMEOUT {
                if let Some(term) = self.term.as_mut() {
                    term.write(CANCEL);
                }
                self.complete_attach();
            }
        }
        if let Some(at) = self.exited_at {
            if !self.pty_eof && now.duration_since(at) > EOF_GRACE {
                self.pty_eof = true;
            }
        }
        if let (Some(at), None, Some(child)) =
            (self.killed_at, self.exit_status, self.child.as_ref())
        {
            if now.duration_since(at) > KILL_GRACE {
                unsafe { libc::kill(-child.pid, libc::SIGKILL) };
            }
        }

        if self.data.as_ref().is_some_and(|d| d.dead) {
            self.data = None;
        }
        let owner_alive = self
            .owner
            .is_some_and(|id| self.clients.iter().any(|c| c.id == id && !c.dead));
        if self.owner.is_some() && !owner_alive {
            self.release_owner(None);
        }

        let finished = self.exit_status.is_some() && self.pty_eof;
        if finished {
            if let (Some(owner), Some(status)) = (self.owner, self.exit_status) {
                let streamed = self.data.as_ref().is_some_and(|d| d.wbuf.is_empty())
                    && self.pending.is_empty();
                if streamed && !self.exit_sent {
                    self.exit_sent = true;
                    self.reply(owner, &Event::Exit { status });
                }
                if self.exit_acked {
                    // The owner knows; ending the data stream ends its reader.
                    self.data = None;
                    return false;
                }
            } else if self.killed_at.is_some() {
                return false;
            }
        }
        if self.killed_at.is_some() && self.exit_status.is_some() && self.owner.is_none() {
            return false;
        }

        self.clients.retain(|c| !c.dead);
        true
    }

    pub fn run(&mut self) -> io::Result<()> {
        loop {
            if !self.tick() {
                break;
            }
            let mut fds: Vec<libc::pollfd> = Vec::new();
            let mut slots: Vec<Slot> = Vec::new();
            let mut add = |fd: RawFd, events: libc::c_short, slot: Slot| {
                fds.push(libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                });
                slots.push(slot);
            };
            add(self.ctl.as_raw_fd(), libc::POLLIN, Slot::Ctl);
            add(
                self.data_listener.as_raw_fd(),
                libc::POLLIN,
                Slot::DataListener,
            );
            for (i, client) in self.clients.iter().enumerate() {
                let mut events = libc::POLLIN;
                if !client.wbuf.is_empty() {
                    events |= libc::POLLOUT;
                }
                add(client.stream.as_raw_fd(), events, Slot::Client(i));
            }
            if let Some(data) = self.data.as_ref() {
                let mut events = 0;
                if self.to_pty.len() < HIGH_WATER {
                    events |= libc::POLLIN;
                }
                if !data.wbuf.is_empty() {
                    events |= libc::POLLOUT;
                }
                add(data.stream.as_raw_fd(), events, Slot::Data);
            }
            if let Some(child) = self.child.as_ref() {
                let mut events = 0;
                if !self.pty_eof && self.owner_backlog() <= HIGH_WATER {
                    events |= libc::POLLIN;
                }
                if !self.to_pty.is_empty() {
                    events |= libc::POLLOUT;
                }
                if events != 0 {
                    add(child.primary.as_raw_fd(), events, Slot::Pty);
                }
                if self.exit_status.is_none() {
                    add(child.pidfd.as_raw_fd(), libc::POLLIN, Slot::PidFd);
                }
            }

            let timeout = 250;
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            for (pollfd, slot) in fds.iter().zip(slots.iter()) {
                let revents = pollfd.revents;
                if revents == 0 {
                    continue;
                }
                match *slot {
                    Slot::Ctl => self.accept_clients(),
                    Slot::DataListener => self.accept_data(),
                    Slot::Client(i) => {
                        if revents & libc::POLLOUT != 0 {
                            self.clients[i].flush();
                        }
                        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                            self.read_client(i);
                        }
                    }
                    Slot::Data => {
                        if let Some(data) = self.data.as_mut() {
                            if revents & libc::POLLOUT != 0 {
                                flush_into(&mut data.stream, &mut data.wbuf, &mut data.dead);
                            }
                        }
                        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                            self.read_data();
                        }
                    }
                    Slot::Pty => {
                        if revents & libc::POLLOUT != 0 {
                            self.write_pty();
                        }
                        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                            self.read_pty();
                        }
                    }
                    Slot::PidFd => self.reap(),
                }
            }
        }
        // Give queued events a last chance to reach their clients.
        for client in self.clients.iter_mut() {
            let _ = client.stream.set_nonblocking(false);
            let _ = client.stream.write_all(&client.wbuf);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.paths.ctl);
        let _ = std::fs::remove_file(&self.paths.data);
        let _ = std::fs::remove_dir(&self.paths.dir);
    }
}

struct SpawnRequest {
    argv: Vec<String>,
    env: Vec<String>,
    cwd: String,
    size: Size,
    scrollback: usize,
    kitty: Kitty,
    name: Option<String>,
}

fn winsize(size: &Size) -> WinSize {
    WinSize {
        rows: size.rows,
        cols: size.cols,
        width_px: (size.cols as u32 * size.cell_width).min(u16::MAX as u32) as u16,
        height_px: (size.rows as u32 * size.cell_height).min(u16::MAX as u32) as u16,
    }
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}
