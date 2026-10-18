//! Applying a whole document state to a live room as one ordinary edit (S6-041, restore).
//!
//! Restoring a named version must not rewrite history or bypass the room: the changes that turn the
//! room's *current* state into the version's state are computed by the collaboration client itself
//! (`Client::edit` diffs the room's document against the target at property granularity) and sent as
//! **one batch** through the relay like any person's edit. The relay admits the sender by the
//! realtime ticket it presents (so the batch is attributed to the person who asked, through the
//! access they hold), commits it as the next sequence number, and broadcasts it, so every connected
//! client sees a normal edit and the log keeps everything that came before.
//!
//! [`apply_document`] is blocking (it speaks the relay's WebSocket protocol with `tungstenite`): run
//! it on a blocking thread. It joins, computes the diff against what the relay sends in its welcome,
//! sends the batch if there is one, waits for the relay's confirmation of that batch, and leaves.

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use figcraft_collab::message::{self, ClientMsg, ServerMsg};
use figcraft_collab::{Client, Document};
use tungstenite::{Bytes, Message};

/// Largest relay message accepted.
const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// What applying a document did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Applied {
    /// The room already held exactly this state: nothing was sent.
    Unchanged,
    /// One batch was accepted as `seq`.
    Batch { seq: u64 },
}

/// Fixed, non-leaking categories for a failed apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyError {
    /// The relay could not be reached or the connection broke.
    Connect,
    /// The relay refused the hello (a bad or expired ticket, no edit right, a room it cannot open).
    Refused,
    /// The room refused the batch.
    Rejected,
    /// The relay did not answer in time.
    Timeout,
    /// The relay sent something unexpected, or the state could not be diffed.
    Protocol,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "restore error: {self:?}")
    }
}

impl std::error::Error for ApplyError {}

type Socket = tungstenite::protocol::WebSocket<TcpStream>;

fn read(socket: &mut Socket, deadline: Instant) -> Result<ServerMsg, ApplyError> {
    loop {
        if Instant::now() >= deadline {
            return Err(ApplyError::Timeout);
        }
        match socket.read() {
            Ok(Message::Binary(bytes)) => return message::decode(&bytes, MAX_MESSAGE_BYTES).map_err(|_| ApplyError::Protocol),
            Ok(Message::Close(_)) => return Err(ApplyError::Connect),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error)) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err(ApplyError::Timeout);
            }
            Err(_) => return Err(ApplyError::Connect),
        }
    }
}

fn send(socket: &mut Socket, msg: &ClientMsg) -> Result<(), ApplyError> {
    let bytes = message::encode(msg).map_err(|_| ApplyError::Protocol)?;
    socket.send(Message::Binary(Bytes::from(bytes))).map_err(|_| ApplyError::Connect)
}

/// Join `file` at the relay at `addr` with `token` (a realtime ticket for a person who may edit),
/// make the room hold `target`, and leave. See the module docs.
pub fn apply_document(addr: SocketAddr, file: &str, token: &str, target: &Document, timeout: Duration) -> Result<Applied, ApplyError> {
    let deadline = Instant::now() + timeout;
    let stream = TcpStream::connect_timeout(&addr, timeout).map_err(|_| ApplyError::Connect)?;
    stream.set_read_timeout(Some(timeout)).map_err(|_| ApplyError::Connect)?;
    stream.set_write_timeout(Some(timeout)).map_err(|_| ApplyError::Connect)?;
    let (mut socket, _) = tungstenite::client::client(format!("ws://{addr}/ws/v1/collab"), stream).map_err(|_| ApplyError::Connect)?;
    send(&mut socket, &Client::hello(file, token, "Restore", ""))?;
    let welcome = loop {
        match read(&mut socket, deadline)? {
            welcome @ ServerMsg::Welcome { .. } => break welcome,
            ServerMsg::Refused { .. } => return Err(ApplyError::Refused),
            _ => {}
        }
    };
    let mut client = Client::join(welcome).map_err(|_| ApplyError::Protocol)?;
    let outcome = match client.edit(target).map_err(|_| ApplyError::Protocol)? {
        None => Applied::Unchanged,
        Some(msg) => {
            let ClientMsg::Changes { batch_id, .. } = &msg else {
                return Err(ApplyError::Protocol);
            };
            let batch_id = *batch_id;
            send(&mut socket, &msg)?;
            let me = client.id();
            loop {
                match read(&mut socket, deadline)? {
                    ServerMsg::Changes(accepted) if accepted.author == me && accepted.batch_id == batch_id => {
                        break Applied::Batch { seq: accepted.seq };
                    }
                    ServerMsg::Reject { batch_id: rejected, .. } if rejected == batch_id => return Err(ApplyError::Rejected),
                    ServerMsg::Refused { .. } => return Err(ApplyError::Refused),
                    _ => {}
                }
            }
        }
    };
    let _ = socket.close(None);
    Ok(outcome)
}
