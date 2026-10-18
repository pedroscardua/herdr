//! Published motionharvest 0.14.0 speaks protocol 12; its current main speaks 13.
//! Only Hello.host_terminal_program changed in requests. All subsequent messages
//! used by machine mode have identical layouts. Support that exact legacy Hello
//! without weakening the server's normal version checks.
use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::protocol::{
    self, ClientKeybindings, ClientLaunchMode, RenderEncoding, ServerMessage, MAX_FRAME_SIZE,
};

#[derive(serde::Serialize)]
enum LegacyMessage {
    Hello {
        version: u32,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        requested_encoding: RenderEncoding,
        keybindings: ClientKeybindings,
        launch_mode: ClientLaunchMode,
    },
}

pub(super) fn handshake(
    stream: &mut UnixStream,
    version: u32,
    cols: u16,
    rows: u16,
) -> io::Result<()> {
    if version == protocol::PROTOCOL_VERSION {
        return match super::do_handshake(
            stream,
            cols,
            rows,
            0,
            0,
            RenderEncoding::SemanticFrame,
            false,
        )
        .map_err(|e| io::Error::other(e.to_string()))?
        {
            RenderEncoding::SemanticFrame => Ok(()),
            _ => Err(io::Error::other(
                "machine server must support semantic frames",
            )),
        };
    }
    if version != 12 {
        return Err(io::Error::other("unsupported saved machine protocol"));
    }
    let hello = LegacyMessage::Hello {
        version,
        cols,
        rows,
        cell_width_px: 0,
        cell_height_px: 0,
        requested_encoding: RenderEncoding::SemanticFrame,
        keybindings: super::requested_keybindings(),
        launch_mode: ClientLaunchMode::App,
    };
    protocol::write_message(stream, &hello).map_err(|e| io::Error::other(e.to_string()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let result = protocol::read_message::<_, ServerMessage>(stream, MAX_FRAME_SIZE);
    stream.set_read_timeout(None)?;
    match result.map_err(|e| io::Error::other(e.to_string()))? {
        ServerMessage::Welcome {
            version: 12,
            encoding: RenderEncoding::SemanticFrame,
            error: None,
        } => Ok(()),
        message => Err(io::Error::other(format!(
            "machine handshake rejected: {message:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_hello_matches_protocol_12_fixture() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            use std::io::Read;
            let mut length = [0; 4];
            server.read_exact(&mut length).unwrap();
            let mut bytes = vec![0; u32::from_le_bytes(length) as usize];
            server.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, vec![0, 12, 80, 23, 0, 0, 0, 0, 0]);
            protocol::write_message(
                &mut server,
                &ServerMessage::Welcome {
                    version: 12,
                    encoding: RenderEncoding::SemanticFrame,
                    error: None,
                },
            )
            .unwrap();
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        handshake(&mut client, 12, 80, 23).unwrap();
        done_tx.send(()).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn unrelated_official_protocol_is_rejected() {
        let (mut client, _) = UnixStream::pair().unwrap();
        assert!(handshake(&mut client, 22, 80, 23).is_err());
    }
}
