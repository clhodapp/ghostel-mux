//! One-shot requests to a session, for the command line.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::paths::SessionPaths;
use crate::protocol::Event;

/// Send REQUEST (a JSON object) to session ID and return its first event.
pub fn request(id: &str, request: &serde_json::Value) -> io::Result<Event> {
    let paths = SessionPaths::new(id);
    let mut stream = UnixStream::connect(&paths.ctl)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    serde_json::from_str(&reply).map_err(io::Error::other)
}
