# ghostel-mux design

ghostel-mux keeps a ghostel terminal's shell alive when the Emacs showing
it goes away, and lets another Emacs take the terminal over with its full
state. This note describes how, and what it does not cover.

## Pieces

| Piece | Where | Does |
|---|---|---|
| session process | `src/` (Rust) | runs one shell on a PTY, keeps a headless libghostty-vt terminal fed with the shell's output, serves one owning Emacs |
| ghostel hooks | `patches/ghostel-native-hooks.patch` | two functions in ghostel's native module: build a terminal from a snapshot, and run the native reader on a Unix socket |
| Emacs side | `lisp/ghostel-mux.el` | starts terminals in sessions, adopts, detaches, lists; advises four ghostel functions |

There is one session process per terminal. A crash takes one terminal with
it, and a new build of the program applies to new terminals without
touching running ones.

## Sockets and files

Each session is a directory `$XDG_RUNTIME_DIR/ghostel-mux/<id>/` holding
two sockets. The parent directory is mode 0700.

- `ctl` carries newline-delimited JSON: requests with an `op` field and
  events with a `type` field. A `snapshot` event line gives a byte count,
  and that many raw snapshot bytes follow it. `src/protocol.rs` defines
  every message.
- `data` is the terminal byte stream of the owner, unframed in both
  directions, so ghostel's reader treats it like a PTY. The session accepts
  one connection per attach, and only from the process whose `ctl`
  connection attached (checked with `SO_PEERCRED`).

Listing the root directory lists the sessions of every Emacs the user
runs. A directory whose `ctl` refuses connections belongs to a session that
died; `ghostel-mux list` removes it.

## Output path

The session is a single-threaded `poll(2)` loop. Output read from the PTY
is written to the owner's `data` connection first and fed to the session's
terminal second. When more than 4 MiB wait for the owner, the session stops
reading the PTY, which slows the shell down the way a slow real terminal
does. With no owner, output only updates the session's terminal.

In Emacs, ghostel's native reader thread reads `data` and parses it under
the terminal lock, exactly as it reads a PTY. Nothing on the output path
runs on Emacs's main thread except rendering, which ghostel already
schedules on a timer.

The table below gives measurements taken on an x86-64 Linux desktop with
release builds.

| Measurement | Session | Bare PTY |
|---|---|---|
| `seq 1 4000000` (35 MB) read from the socket or PTY | 31–34 MB/s | 29–31 MB/s |
| ghostel terminal, `seq 1 2000000`, until the last line is in the terminal | 25–26 MB/s | 25–27 MB/s (native PTY) |
| one typed byte until its echo returns, median / p99 | 6–7 µs / 16–22 µs | 3.5 µs / 8–10 µs |

The kernel PTY bounds throughput, not the session. The extra echo latency
is one Unix-socket hop each way, against ghostel's 33 ms redraw interval.
`test/session_bench.py`, `test/latency_bench.py` and
`test/throughput-bench.el` reproduce the numbers.

## Attaching: the snapshot cut

An attach makes the requester the owner, then sends a snapshot of the
session's terminal (libghostty's binary snapshot format: both screens,
full scrollback, modes, palette overrides, title, working directory,
Kitty keyboard state, semantic-prompt state), then the `attached` event.
The owner connects to `data` and receives every byte that followed the
cut, including bytes that arrived before it connected.

A snapshot has to be cut where the VT parser is between sequences, because
ghostel's native reader starts a fresh parser. When an attach arrives
mid-sequence, the session feeds further PTY output into its terminal with
`ghostty_terminal_vt_write_until_ground` and cuts at the first byte where
the parser reaches ground; the rest of that chunk goes to the owner. If the
parser has not reached ground after one second, or the PTY has already
closed, the session cancels the unfinished sequence in the session's terminal
with CAN and cuts there.

In Emacs, `ghostel-mux-adopt` binds the snapshot and lets ghostel create
its buffer as usual. Its advice on `ghostel--new` builds the terminal with
`ghostel--new-from-snapshot` instead, so ghostel's usual setup (window size,
face palette, bold settings) applies on top of the restored state. The
snapshot's title and directory reach ghostel's Lisp state through
`ghostel--set-title` and `ghostel--update-directory`.

## Ownership

A session has at most one owner, the Emacs whose `ctl` connection last
attached or spawned. An attach to an owned session answers `busy` with the
owner's name, unless the request sets `steal`. Stealing sends the old owner
`stolen`, closes its connections, and attaches the new one. `evict`, from
any client, sends the owner `evicted` and leaves the session detached;
`ghostel-mux evict ID` is the command-line form. An owner's buffer that
loses its session keeps its text and says who took it.

While no Emacs owns a session, the session's terminal answers the shell's
terminal queries (device attributes, cursor position), so programs that
ask do not hang. While an Emacs owns it, ghostel's terminal answers them
and the session's terminal stays silent.

## Lifetimes

- **Start.** `ghostel-mux start` creates an id and starts `ghostel-mux
  session ID` outside the caller. When the systemd user manager is
  reachable, the session runs as the transient service
  `ghostel-mux-<id>.service` (`systemd-run --user`, `Type=notify`), so
  stopping the Emacs service does not kill it. Otherwise it detaches by
  forking twice. Either way `start` returns once the sockets are bound.
  `GHOSTEL_MUX_LAUNCHER=systemd|fork` overrides the choice. ghostel-mux-nix
  has a NixOS VM test that runs an Emacs as a user service, stops the
  service, and adopts the terminal from another Emacs; it also shows that a
  session started with the fork launcher dies with that service.
- **Spawn.** The first request on a new session is `spawn`, carrying the
  argv, environment, directory and size that ghostel computed, so ghostel's
  shell integration works unchanged. A session that gets no `spawn` within
  30 seconds exits.
- **Emacs goes away.** Its `ctl` connection closes and the session becomes
  detached. Nothing in Emacs sends SIGKILL to the shell: ghostel's
  `kill-emacs-hook` only signals native children it forked, and a session's
  shell is not one.
- **Killing the buffer** sends `kill`: the session sends SIGHUP to the
  terminal's foreground process group and to the shell's group, SIGKILL
  after five more seconds, and exits once the shell is gone.
  `ghostel-mux-detach` closes the buffer without `kill`.
- **The shell exits.** The session drains the PTY (up to two seconds after
  the exit, for processes that still hold it open), sends `exit` with the
  status on `ctl`, waits for `exit-ack`, then closes `data` and exits.
  The ordering lets the buffer report the real status: ghostel's reader sees
  only a closed socket, and `ghostel-mux.el` passes ghostel's sentinel the
  status from `exit`. A shell that exits while the session is detached
  leaves the session running with its final screen until an Emacs adopts it
  or someone kills it.

## The hooks in ghostel

`ghostel--connect-native-socket TERM PATH PIPE &optional TTY-NAME` reuses
`PosixPtyProcess.zig` with a connected socket in place of the PTY primary:
no fork, resize and the foreground-group query do nothing, writes use
`send` with `MSG_NOSIGNAL`, end of file closes the reader, and the exit
status written to PIPE is 0. TTY-NAME names the session's PTY, so ghostel's
password-prompt detection reads the right terminal's echo flag.

The patch is made against ghostel 0.60.0.

`ghostel--new-from-snapshot SNAPSHOT &optional KITTY-STORAGE-LIMIT
KITTY-MEDIUMS` decodes a ground-state snapshot into a terminal and wraps it
the way `ghostel--new` wraps a new one. Both functions are generic; neither
knows about sessions. What ghostel-mux needs beyond them is advice in
`ghostel-mux.el`:

| Advised | Why |
|---|---|
| `ghostel--spawn-process` | starts local shells in sessions |
| `ghostel--new` | builds the terminal from a pending snapshot |
| `ghostel--set-size` | sends resizes, with cell pixel sizes, to the session |
| `ghostel-foreground-pid` | reads the shell's foreground group from `/proc/PID/stat`, since Emacs holds no descriptor for the session's PTY |

## Kitty graphics images

From ghostty `b2c78c9` on, which ghostel 0.60.0 vendors, a snapshot
carries each screen's Kitty graphics images, their placements and their
pixels. An image on screen at a detach is drawn again after an adopt. The
session's terminal keeps images under the settings the owning Emacs sent at
spawn: `ghostel-kitty-graphics-storage-limit` and
`ghostel-kitty-graphics-mediums`. The `ghostel-mux-kitty-image-survives-adopt`
test checks that a snapshot taken after a detach gives ghostel an image to
draw.

## Snapshot compatibility

The snapshot format carries no promise across ghostty commits, so the
program has to be built against libghostty-vt from the commit ghostel
vendors. ghostel-mux-nix does that, and one of its checks fails when the
two commits diverge. A session started before an upgrade can
still meet an Emacs built after it. Before `ghostel-mux-adopt` creates a
buffer, it decodes the snapshot once; when that fails, it asks whether to
adopt the session anyway and says that the screen and scrollback will be
lost. Declining leaves the session detached and unchanged. Accepting shows
the session in a blank terminal from then on.

## Not covered

- **Following another Emacs's exit.** An Emacs adopts detached sessions
  when asked, or when `ghostel-mux-mode` starts with
  `ghostel-mux-adopt-on-startup` set. Nothing watches for sessions that
  become detached later, as happens when a rotated daemon drains after its
  replacement has started.
- **Platforms.** The session uses `pidfd_open`, `SO_PEERCRED` and
  `/proc`, so it is Linux only.
