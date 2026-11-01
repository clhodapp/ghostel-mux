//! ghostel-mux: terminal sessions for ghostel that outlive the Emacs showing
//! them.
//!
//! Usage:
//!   ghostel-mux start                start a session and print its id
//!   ghostel-mux list [--json]        every session of this user
//!   ghostel-mux info ID              one session's metadata as JSON
//!   ghostel-mux evict ID             force the owning Emacs off a session
//!   ghostel-mux kill ID              hang up a session's terminal
//!   ghostel-mux name ID [NAME]       set or clear a session's name
//!   ghostel-mux session ID           run a session (started by `start`)

mod client;
mod ffi;
mod launch;
mod paths;
mod protocol;
mod pty;
mod session;
mod terminal;

use std::io::{self, ErrorKind};
use std::process::ExitCode;

use serde_json::json;

use crate::protocol::{Event, Info};

fn usage() -> ExitCode {
    eprintln!(
        "usage: ghostel-mux start | list [--json] | info ID | evict ID | kill ID | name ID [NAME]"
    );
    ExitCode::from(2)
}

fn run_session(args: &[String]) -> io::Result<()> {
    let id = args
        .first()
        .ok_or_else(|| io::Error::other("missing session id"))?;
    if !paths::valid_id(id) {
        return Err(io::Error::other("invalid session id"));
    }
    let ready_fd = match args.get(1).map(String::as_str) {
        Some("--ready-fd") => Some(
            args.get(2)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| io::Error::other("bad --ready-fd"))?,
        ),
        _ => None,
    };
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
    paths::ensure_root()?;
    let mut session = session::Session::bind(id, paths::SessionPaths::new(id))?;
    launch::notify_ready(ready_fd);
    session.run()
}

/// Metadata of every reachable session.  A session directory whose socket
/// refuses connections belongs to a session that died; it is removed.
fn all_sessions() -> io::Result<Vec<Info>> {
    let mut infos = Vec::new();
    for id in paths::session_ids()? {
        match client::request(&id, &json!({"op": "info"})) {
            Ok(Event::Info { info }) => infos.push(info),
            Err(err) if err.kind() == ErrorKind::ConnectionRefused => {
                let dir = paths::SessionPaths::new(&id).dir;
                let _ = std::fs::remove_dir_all(dir);
            }
            _ => {}
        }
    }
    infos.sort_by_key(|info| info.created);
    Ok(infos)
}

fn print_table(infos: &[Info]) {
    println!(
        "{:<17} {:<8} {:<24} {:<10} TITLE",
        "ID", "PID", "OWNER", "STATE"
    );
    for info in infos {
        let state = match (info.exit_status, &info.owner) {
            (Some(code), _) => format!("exit {code}"),
            (None, Some(_)) => "attached".to_string(),
            (None, None) => "detached".to_string(),
        };
        let owner = info
            .owner
            .as_ref()
            .map(|o| o.client.clone())
            .unwrap_or_default();
        let pid = info.pid.map(|p| p.to_string()).unwrap_or_default();
        let title = if info.title.is_empty() {
            info.argv.join(" ")
        } else {
            info.title.clone()
        };
        println!(
            "{:<17} {:<8} {:<24} {:<10} {}",
            info.id, pid, owner, state, title
        );
    }
}

fn expect_ok(event: Event) -> io::Result<()> {
    match event {
        Event::Ok => Ok(()),
        Event::Error { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected reply: {other:?}"))),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        return usage();
    };
    let rest = &args[1..];
    let result: io::Result<()> = match command.as_str() {
        "session" => run_session(rest),
        "start" => (|| {
            paths::ensure_root()?;
            let id = paths::new_id()?;
            launch::start(&id, launch::Launcher::choose())?;
            println!("{id}");
            Ok(())
        })(),
        "list" => all_sessions().map(|infos| {
            if rest.first().map(String::as_str) == Some("--json") {
                println!(
                    "{}",
                    serde_json::to_string(&infos).expect("infos serialize")
                );
            } else {
                print_table(&infos);
            }
        }),
        "info" | "evict" | "kill" | "name" => {
            let Some(id) = rest.first() else {
                return usage();
            };
            if !paths::valid_id(id) {
                return usage();
            }
            match command.as_str() {
                "info" => {
                    client::request(id, &json!({"op": "info"})).and_then(|event| match event {
                        Event::Info { info } => {
                            println!("{}", serde_json::to_string(&info).expect("info serializes"));
                            Ok(())
                        }
                        other => Err(io::Error::other(format!("unexpected reply: {other:?}"))),
                    })
                }
                "evict" => client::request(id, &json!({"op": "evict"})).and_then(expect_ok),
                "kill" => client::request(id, &json!({"op": "kill"})).and_then(expect_ok),
                _ => client::request(id, &json!({"op": "set-name", "name": rest.get(1)}))
                    .and_then(expect_ok),
            }
        }
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ghostel-mux: {err}");
            ExitCode::FAILURE
        }
    }
}
