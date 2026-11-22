//! WebSocket connection to OpenDeck (Stream Deck SDK protocol).
//!
//! One network thread owns the socket: it sends queued outgoing messages and forwards
//! incoming text messages. The rest of the plugin only talks to it through channels, so
//! any thread can send without blocking on a read in progress.

use crate::log;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;
use tungstenite::{Error, Message};

/// Connects to OpenDeck. Returns (outgoing sender, incoming receiver). The incoming
/// channel closes when the connection ends.
pub fn connect(port: u16) -> Result<(Sender<String>, Receiver<String>), String> {
    let stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let (mut socket, _) =
        tungstenite::client(format!("ws://127.0.0.1:{port}"), stream).map_err(|e| e.to_string())?;
    // Short read timeout: the thread alternates between reading and sending.
    socket.get_mut().set_read_timeout(Some(Duration::from_millis(10))).map_err(|e| e.to_string())?;

    let (out_tx, out_rx) = mpsc::channel::<String>();
    let (in_tx, in_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        loop {
            // 1. Send everything that is queued.
            loop {
                match out_rx.try_recv() {
                    Ok(text) => {
                        if let Err(e) = socket.write(Message::text(text)) {
                            log!("websocket write error: {e}");
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            match socket.flush() {
                Ok(()) => {}
                Err(Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => {
                    log!("websocket flush error: {e}");
                    return;
                }
            }
            // 2. Read one message, or time out.
            match socket.read() {
                Ok(Message::Text(text)) => {
                    if in_tx.send(text.to_string()).is_err() {
                        return;
                    }
                }
                Ok(Message::Close(_)) => {
                    log!("websocket closed by OpenDeck");
                    return;
                }
                Ok(_) => {} // ping / pong / binary: pongs are answered by tungstenite
                Err(Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(Error::ConnectionClosed | Error::AlreadyClosed) => return,
                Err(e) => {
                    log!("websocket read error: {e}");
                    return;
                }
            }
        }
    });
    Ok((out_tx, in_rx))
}
