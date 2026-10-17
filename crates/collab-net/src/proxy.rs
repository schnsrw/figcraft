//! A connection the hub passes on to the replica that owns its file's room (S5-006).
//!
//! The client's connection stays with this hub; a second one is opened to the owner (through TLS
//! for `wss://`), the client's hello is sent on it, and from then on every client message goes up
//! and everything the owner says comes back down unchanged. The owner admits the client itself
//! (it checks the token), so a proxied client is held to exactly the owner's rules. Nothing here
//! looks inside what passes except to encode it again.

use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

use figcraft_collab::message::{self, ClientMsg};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, Role as WsRole, WebSocket};
use tungstenite::{Bytes, Message};

use crate::invite::Endpoint;
use crate::net::{NetLimits, config, open, shutdown};

/// How long to wait for the owner to answer the connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Client messages waiting for the owner's connection; a client that gets this far ahead of the
/// owner is dropped (it reconnects).
const UP_QUEUE: usize = 256;

/// What comes back from the owner for the client.
pub(crate) enum Down {
    /// One message, as the owner sent it.
    Bytes(Bytes),
    /// The owner closed the connection (or it broke): close the client's the same way.
    Close(CloseCode, String),
}

/// Connect to `owner`, send `hello` and start passing messages. `down` hands what the owner says
/// to the client's writer; it returns false when the client can't take more (the proxy ends).
/// The returned sender takes the client's later messages; dropping it ends the proxy.
pub(crate) fn start(owner: &Endpoint, hello: &ClientMsg, down: impl Fn(Down) -> bool + Send + 'static) -> Result<SyncSender<ClientMsg>, String> {
    let limits = NetLimits::default();
    let (rd, wr, sock) = open(owner, CONNECT_TIMEOUT, &limits)?;
    let (mut reader, _) = tungstenite::client::client_with_config(owner.display(), rd, Some(config(limits.max_server_message)))
        .map_err(|e| format!("{} doesn't answer as a FigCraft session: {e}", owner.display()))?;
    let mut writer = WebSocket::from_raw_socket(wr, WsRole::Client, Some(config(limits.max_client_message)));
    let first = message::encode(hello).map_err(|e| e.to_string())?;
    writer.send(Message::Binary(Bytes::from(first))).map_err(|e| e.to_string())?;
    reader.get_ref().set_read_timeout(Some(limits.idle_timeout)).map_err(|e| e.to_string())?;

    let (up_tx, up_rx) = sync_channel::<ClientMsg>(UP_QUEUE);
    let read_sock = sock.try_clone().map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("hub-proxy-down".into())
        .spawn(move || {
            let end = loop {
                match reader.read() {
                    Ok(Message::Binary(b)) => {
                        if !down(Down::Bytes(b)) {
                            break None;
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        let (code, why) = frame.map_or((CloseCode::Away, String::new()), |f| (f.code, f.reason.to_string()));
                        break Some((code, why));
                    }
                    Ok(_) => {}
                    Err(_) => break Some((CloseCode::Restart, "the connection to the document's server was lost".to_string())),
                }
            };
            if let Some((code, why)) = end {
                down(Down::Close(code, why));
            }
            shutdown(&read_sock);
        })
        .map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("hub-proxy-up".into())
        .spawn(move || {
            loop {
                let msg = match up_rx.recv_timeout(limits.keepalive) {
                    Ok(m) => m,
                    // Quiet client: keep the owner's idle timer fed.
                    Err(RecvTimeoutError::Timeout) => ClientMsg::Ping,
                    Err(RecvTimeoutError::Disconnected) => {
                        let _ = writer.close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() }));
                        break;
                    }
                };
                let sent = message::encode(&msg).is_ok_and(|b| writer.send(Message::Binary(Bytes::from(b))).is_ok());
                if !sent {
                    break;
                }
            }
            shutdown(&sock);
        })
        .map_err(|e| e.to_string())?;
    Ok(up_tx)
}
