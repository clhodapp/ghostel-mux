# ghostel-mux

Terminals for [ghostel](https://github.com/dakra/ghostel) that outlive the
Emacs showing them. Each terminal's shell runs in its own small session
process, outside Emacs. When Emacs exits, crashes, or is replaced by a
newer daemon, the shell keeps running. Another Emacs can then adopt the
terminal with its screen, scrollback, title and modes intact, and ghostel
behaves as it does for any other terminal: native Emacs scrolling, search
and copy over the scrollback, prompt navigation, links, and the rest.

A session has at most one owning Emacs at a time. The sessions of every
Emacs the user runs are listed in one place, and from there a session can
be adopted, taken from the Emacs that owns it, forced off its owner, or
killed.

## How it works

The session process holds the shell's PTY and a headless copy of the
terminal, built from libghostty-vt, the same library ghostel's native
module uses. Every byte the shell writes goes first to the owning Emacs and
then into the session's copy. When an Emacs attaches, the session sends a
libghostty snapshot of its copy, cut at an exact point in the output, and
then everything after that point. Ghostel builds its terminal from the
snapshot, so nothing is replayed or redrawn.

On the Emacs side, the bytes arrive on ghostel's own native reader thread,
as PTY output does, and are parsed off the main thread. Two hooks in
ghostel's native module make that possible: `ghostel--new-from-snapshot`
and `ghostel--connect-native-socket`. They live in
`patches/ghostel-native-hooks.patch`. Everything else is in
`lisp/ghostel-mux.el` and the `ghostel-mux` program.

[`docs/development/design.md`](docs/development/design.md) has the
details: the protocol, ownership, the snapshot cut, process lifetimes, and
what is not covered.

## Build

The program is a Rust crate. It links libghostty-vt, which it finds
through pkg-config, and generates its bindings with bindgen, which needs
libclang:

```sh
cargo build --release
```

libghostty-vt has to come from the same ghostty commit as the one
ghostel's native module vendors, because the snapshot format a session
writes has no compatibility promise across ghostty commits. For ghostel
0.60.0 that is ghostty `b2c78c9`.

Ghostel needs `patches/ghostel-native-hooks.patch` applied to its source
before its native module is built. The patch is made against ghostel
0.60.0.

[ghostel-mux-nix](https://github.com/clhodapp/ghostel-mux-nix) builds all
of this with Nix: libghostty-vt from the right ghostty commit, the
program, the Lisp, and the patch for an Emacs package set to apply.

## Use it

Put `lisp/` on the load path and `ghostel-mux` on `PATH`, or set
`ghostel-mux-program` to the program. Then, in Emacs:

```elisp
(require 'ghostel-mux)
(ghostel-mux-mode 1)
```

With the mode on, every local terminal ghostel starts runs in a session.
Remote (TRAMP) terminals are not affected.

| Command | Does |
|---|---|
| `ghostel-mux-list` | lists every session; `RET` adopts, `s` takes, `e` forces the owner off, `k` kills, `o` adopts the orphans, `A` adopts all |
| `ghostel-mux-adopt` | adopts a session; with a prefix argument, takes it from its owner |
| `ghostel-mux-adopt-detached` | adopts every orphaned terminal: each detached session whose shell still runs |
| `ghostel-mux-adopt-all` | adopts every terminal whose shell still runs, taking those other Emacsen own |
| `ghostel-mux-detach` | closes the current terminal buffer and leaves its session running |

Evil bindings for `ghostel-mux-list` are in a separate file,
`ghostel-mux-evil.el`, next to `ghostel-mux.el`. Without it,
the list keeps its plain Emacs keys. To use it, load it after Evil:

```elisp
(with-eval-after-load 'evil
  (require 'ghostel-mux-evil))
```

With it, the list starts in normal state. `j` and `k` move between rows,
and these keys act on the sessions:

| Key | Does |
|---|---|
| `RET` | adopts the session |
| `s` | takes it from its owner |
| `E` | evicts the owner |
| `D` | kills the session |
| `o` | adopts every orphaned terminal |
| `A` | adopts every terminal |
| `g r` | refreshes the list |
| `q` | quits |

A session started by a ghostel-mux built with a different terminal
library cannot have its screen and scrollback restored. Adopting one asks
first; in the two commands that adopt several sessions, the prompt can also
answer for every remaining such session in that command.

Killing a terminal buffer ends its session, as it ends the shell today.
Setting `ghostel-mux-adopt-on-startup` adopts the detached sessions when
the mode is turned on.

The program has a command line of its own:

```sh
ghostel-mux list            # every session, with owner and state
ghostel-mux evict ID        # force the owning Emacs off a session
ghostel-mux kill ID         # hang up a session's terminal
```

## Test

Each test suite runs against a built program, given in `GHOSTEL_MUX`, and
starts its sessions with the fork launcher in a private runtime directory.

The protocol tests need Python 3:

```sh
GHOSTEL_MUX=$PWD/target/release/ghostel-mux python3 test/session_test.py
```

The end-to-end tests need a batch Emacs whose ghostel carries the patch,
plus Evil for the tests of `ghostel-mux-evil.el`:

```sh
XDG_RUNTIME_DIR=$(mktemp -d) GHOSTEL_MUX=$PWD/target/release/ghostel-mux \
  emacs --batch -L lisp -l test/ghostel-mux-test.el -f ert-run-tests-batch-and-exit
```

The hook tests check the patch alone. `test/hooks/mksnap.c`, built against
libghostty-vt, writes their snapshot fixture:

```sh
cc -o mksnap test/hooks/mksnap.c $(pkg-config --cflags --libs libghostty-vt)
./mksnap test/hooks/fixture.snap
emacs --batch -l test/hooks/hooks-test.el -f ert-run-tests-batch-and-exit
```

`test/session_bench.py`, `test/latency_bench.py` and
`test/throughput-bench.el` measure throughput and latency.
ghostel-mux-nix runs every suite as a flake check, together with a NixOS
VM test of the systemd launcher.

## License

The Emacs Lisp in `lisp/` and the ghostel patch in `patches/` are licensed
under the GNU General Public License, version 3 or later; `COPYING` holds
the license text. The patch modifies ghostel, which is GPL-licensed, and
the Lisp runs inside Emacs, which is too.

Everything else, including the session program in `src/` and the tests,
is licensed under the MIT License in `LICENSE`.
