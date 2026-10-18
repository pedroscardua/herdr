//! Client-side machine switching over the fork's existing semantic-frame protocol.
//! Each connection owns its input stream and frame. IDs and remote paths never
//! enter the local AppState, so identical w1:p1 IDs on different servers are safe.
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

mod view;
use super::{ClientLoopEvent, ServerMessage};
use crate::machine::{ssh, Catalog, Machine};
use crate::protocol::{self, ClientMessage, FrameData, NotifyKind, MAX_FRAME_SIZE};
use view::{clean, compose, content_rows, InputAction, InputRouter};
#[cfg(test)]
use view::{empty_frame, input_action, tabs};

pub(super) enum Event {
    Connected {
        id: String,
        generation: u64,
        result: Result<Connection, String>,
    },
    Message {
        id: String,
        generation: u64,
        message: ServerMessage,
    },
    Disconnected {
        id: String,
        generation: u64,
        error: String,
    },
}

pub(super) struct Connection {
    stream: UnixStream,
    tunnel: Option<ssh::Tunnel>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Closing just this client's connection never stops the server or panes.
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

struct Endpoint {
    id: String,
    label: String,
    profile: Option<Machine>,
    generation: u64,
    connecting: bool,
    connection: Option<Connection>,
    frame: Option<FrameData>,
    error: Option<String>,
    retry: Instant,
    failures: u32,
    unread: bool,
    mouse_capture: bool,
}

impl Endpoint {
    fn local() -> Self {
        Self::new("local".into(), "Local".into(), None)
    }
    fn new(id: String, label: String, profile: Option<Machine>) -> Self {
        Self {
            id,
            label,
            profile,
            generation: 0,
            connecting: false,
            connection: None,
            frame: None,
            error: None,
            retry: Instant::now(),
            failures: 0,
            unread: false,
            mouse_capture: true,
        }
    }
    fn disconnect(&mut self, error: String) {
        self.connection.take();
        self.frame = None;
        self.connecting = false;
        self.error = Some(error);
        self.failures = self.failures.saturating_add(1);
        self.retry = Instant::now() + Duration::from_secs((2u64 << self.failures.min(5)).min(60));
    }
    fn send(&mut self, message: &ClientMessage) {
        if let Some(c) = &mut self.connection {
            if let Err(e) = super::write_to_server(&mut c.stream, message) {
                self.disconnect(e.to_string());
            }
        }
    }
    fn ready(&self, cols: u16, rows: u16) -> bool {
        self.connection.is_some()
            && self
                .frame
                .as_ref()
                .is_some_and(|f| f.width == cols && f.height == rows)
    }
}

fn launch(endpoint: &mut Endpoint, cols: u16, rows: u16, tx: &mpsc::Sender<ClientLoopEvent>) {
    endpoint.generation = endpoint.generation.wrapping_add(1);
    endpoint.connecting = true;
    let generation = endpoint.generation;
    let id = endpoint.id.clone();
    let profile = endpoint.profile.clone();
    let local_socket = crate::server::socket_paths::client_socket_path();
    let local_api = crate::api::socket_path();
    let tx = tx.clone();
    std::thread::spawn(move || {
        let result = (|| {
            let protocol = if let Some(profile) = &profile {
                profile.protocol
            } else {
                crate::api::read_runtime_status_at(&local_api, Duration::from_secs(1))?
                    .and_then(|s| s.protocol)
                    .unwrap_or(protocol::PROTOCOL_VERSION)
            };
            let (mut stream, tunnel) = if let Some(profile) = profile {
                let (stream, tunnel) = ssh::connect(&profile)?;
                (stream, Some(tunnel))
            } else {
                (UnixStream::connect(local_socket)?, None)
            };
            stream.set_write_timeout(Some(Duration::from_secs(1)))?;
            // Timeouts bound failed SSH setup; the UI and Local remain usable.
            let handshake = super::machine_handshake::handshake(&mut stream, protocol, cols, rows);
            match handshake {
                Ok(()) => Ok(Connection { stream, tunnel }),
                Err(e) => Err(io::Error::other(format!(
                    "{e} {}",
                    tunnel.as_ref().map(|t| t.error()).unwrap_or_default()
                ))),
            }
        })()
        .map_err(|e: io::Error| e.to_string());
        let _ = tx.blocking_send(ClientLoopEvent::Machine(Event::Connected {
            id,
            generation,
            result,
        }));
    });
}

fn reader(endpoint: &Endpoint, tx: &mpsc::Sender<ClientLoopEvent>) -> io::Result<()> {
    let Some(connection) = &endpoint.connection else {
        return Ok(());
    };
    let mut stream = connection.stream.try_clone()?;
    let id = endpoint.id.clone();
    let generation = endpoint.generation;
    let tx = tx.clone();
    std::thread::spawn(move || loop {
        match protocol::read_message(&mut stream, MAX_FRAME_SIZE) {
            Ok(message) => {
                if tx
                    .blocking_send(ClientLoopEvent::Machine(Event::Message {
                        id: id.clone(),
                        generation,
                        message,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.blocking_send(ClientLoopEvent::Machine(Event::Disconnected {
                    id,
                    generation,
                    error: e.to_string(),
                }));
                break;
            }
        }
    });
    Ok(())
}

fn reconcile(endpoints: &mut Vec<Endpoint>, catalog: &Catalog, selected: &mut String) {
    endpoints.retain(|e| {
        e.profile.is_none() || catalog.machines.iter().any(|m| m.enabled && m.id == e.id)
    });
    for machine in catalog.machines.iter().filter(|m| m.enabled) {
        if let Some(endpoint) = endpoints.iter_mut().find(|e| e.id == machine.id) {
            let transport_changed = endpoint.profile.as_ref().is_some_and(|m| {
                m.target != machine.target
                    || m.session != machine.session
                    || m.protocol != machine.protocol
            });
            if transport_changed {
                endpoint.disconnect("SSH profile changed".into());
                endpoint.generation = endpoint.generation.wrapping_add(1);
                endpoint.retry = Instant::now();
            }
            endpoint.label = machine.label.clone();
            endpoint.profile = Some(machine.clone());
        } else {
            endpoints.push(Endpoint::new(
                machine.id.clone(),
                machine.label.clone(),
                Some(machine.clone()),
            ));
        }
    }
    if !endpoints.iter().any(|e| e.id == *selected) {
        *selected = "local".into();
    }
}

pub(super) fn run() -> io::Result<()> {
    super::init_logging();
    let loaded = crate::config::Config::load().config;
    let catalog = Catalog::load()?;
    let (cols, rows, _, _) = super::current_terminal_geometry(false);
    let _guard = super::setup_terminal(true)?;
    let quit = Arc::new(AtomicBool::new(false));
    let ctrlc_quit = quit.clone();
    let _ = ctrlc::set_handler(move || {
        ctrlc_quit.store(true, Ordering::Release);
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run_loop(cols, rows, catalog, quit.clone(), loaded));
    quit.store(true, Ordering::Release);
    runtime.shutdown_timeout(Duration::from_millis(100));
    result
}

async fn run_loop(
    mut cols: u16,
    mut rows: u16,
    mut catalog: Catalog,
    quit: Arc<AtomicBool>,
    config: crate::config::Config,
) -> io::Result<()> {
    let (tx, mut rx) = mpsc::channel(256);
    let input_tx = tx.clone();
    let input_quit = quit.clone();
    std::thread::spawn(move || super::input::stdin_reader_loop(input_tx, &input_quit));
    let resize_tx = tx.clone();
    let resize_quit = quit.clone();
    std::thread::spawn(move || super::resize_poll_loop(resize_tx, cols, rows, false, &resize_quit));
    super::query_host_terminal_theme();
    let mut endpoints = vec![Endpoint::local()];
    let mut selected = "local".to_string();
    reconcile(&mut endpoints, &catalog, &mut selected);
    let mut bar = endpoints.len() > 1;
    let mut encoder = super::render_ansi::BlitEncoder::new();
    let mut reload_at = Instant::now();
    let mut dirty = true;
    let mut capture = true;
    let mut router = InputRouter::default();
    let mut sound = config.ui.sound;
    let mut redraw_focus = config.ui.redraw_on_focus_gained;
    while !quit.load(Ordering::Acquire) {
        if Instant::now() >= reload_at {
            match Catalog::load() {
                Ok(next) if next != catalog => {
                    catalog = next;
                    reconcile(&mut endpoints, &catalog, &mut selected);
                    bar = endpoints.len() > 1;
                    for e in &mut endpoints {
                        e.send(&ClientMessage::Resize {
                            cols,
                            rows: content_rows(rows, bar),
                            cell_width_px: 0,
                            cell_height_px: 0,
                        });
                    }
                    encoder = super::render_ansi::BlitEncoder::new();
                    dirty = true;
                }
                Err(e) => {
                    tracing::warn!(%e, "keeping existing SSH profiles after catalog read failure")
                }
                _ => {}
            }
            reload_at = Instant::now() + Duration::from_secs(1);
        }
        for endpoint in &mut endpoints {
            if endpoint.connection.is_none()
                && !endpoint.connecting
                && Instant::now() >= endpoint.retry
            {
                launch(endpoint, cols, content_rows(rows, bar), &tx);
                dirty = true;
            }
        }
        if dirty {
            let frame = compose(&endpoints, &selected, cols, rows, bar);
            let encoded = encoder.encode(&frame, false);
            let mut stdout = io::stdout().lock();
            stdout.write_all(&encoded.bytes)?;
            stdout.flush()?;
            encoder.commit(frame, encoded);
            let desired = bar
                || endpoints
                    .iter()
                    .find(|e| e.id == selected)
                    .is_some_and(|e| e.mouse_capture);
            if desired != capture {
                super::set_mouse_capture(desired)?;
                capture = desired;
            }
            dirty = false;
        }
        let event = tokio::select! {
            e = rx.recv() => e.unwrap_or(ClientLoopEvent::Timer),
            _ = tokio::time::sleep(Duration::from_millis(100)) => ClientLoopEvent::Timer,
        };
        match event {
            ClientLoopEvent::StdinInput(data) => {
                match router.action(&data, &endpoints, &selected, (cols, rows), bar) {
                    InputAction::Select(id) => {
                        selected = id;
                        if let Some(e) = endpoints.iter_mut().find(|e| e.id == selected) {
                            e.unread = false;
                        }
                        encoder = super::render_ansi::BlitEncoder::new();
                        dirty = true;
                        continue;
                    }
                    InputAction::Consume => continue,
                    InputAction::Forward => {}
                }
                let events = crate::raw_input::parse_raw_input_bytes_sync(&data);
                if crate::raw_input::events_require_host_surface_redraw(&events, redraw_focus) {
                    encoder = super::render_ansi::BlitEncoder::new();
                    dirty = true;
                }
                if let Some(endpoint) = endpoints.iter_mut().find(|e| e.id == selected) {
                    // Do not send input into disconnected or stale cached surfaces.
                    if !endpoint.ready(cols, content_rows(rows, bar)) {
                        continue;
                    }
                    let message = if super::should_bridge_clipboard_image_paste(&data) {
                        match crate::platform::read_clipboard_image() {
                            Some(image)
                                if image.bytes.len() <= protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD =>
                            {
                                ClientMessage::ClipboardImage {
                                    extension: image.extension.to_owned(),
                                    data: image.bytes,
                                }
                            }
                            Some(_) => {
                                super::handle_notify(
                                    NotifyKind::Toast,
                                    "clipboard image is too large",
                                    &sound,
                                );
                                continue;
                            }
                            None => ClientMessage::Input { data },
                        }
                    } else {
                        ClientMessage::Input { data }
                    };
                    endpoint.send(&message);
                    dirty = true;
                }
            }
            ClientLoopEvent::Resize(new_cols, new_rows, _, _) => {
                cols = new_cols;
                rows = new_rows;
                for e in &mut endpoints {
                    e.send(&ClientMessage::Resize {
                        cols,
                        rows: content_rows(rows, bar),
                        cell_width_px: 0,
                        cell_height_px: 0,
                    });
                }
                encoder = super::render_ansi::BlitEncoder::new();
                dirty = true;
            }
            ClientLoopEvent::Machine(Event::Connected {
                id,
                generation,
                result,
            }) => {
                if let Some(e) = endpoints
                    .iter_mut()
                    .find(|e| e.id == id && e.generation == generation)
                {
                    e.connecting = false;
                    match result {
                        Ok(connection) => {
                            e.connection = Some(connection);
                            e.error = None;
                            e.failures = 0;
                            e.send(&ClientMessage::Resize {
                                cols,
                                rows: content_rows(rows, bar),
                                cell_width_px: 0,
                                cell_height_px: 0,
                            });
                            if let Err(error) = reader(e, &tx) {
                                e.disconnect(error.to_string());
                            }
                        }
                        Err(error) => e.disconnect(error),
                    }
                    dirty = true;
                }
            }
            ClientLoopEvent::Machine(Event::Disconnected {
                id,
                generation,
                error,
            }) => {
                if let Some(e) = endpoints
                    .iter_mut()
                    .find(|e| e.id == id && e.generation == generation)
                {
                    let ssh_error = e
                        .connection
                        .as_ref()
                        .and_then(|c| c.tunnel.as_ref())
                        .map(|t| t.error())
                        .unwrap_or_default();
                    e.disconnect(format!("{error} {ssh_error}"));
                    dirty = true;
                }
            }
            ClientLoopEvent::Machine(Event::Message {
                id,
                generation,
                message,
            }) => {
                if let Some(e) = endpoints
                    .iter_mut()
                    .find(|e| e.id == id && e.generation == generation && e.connection.is_some())
                {
                    match message {
                        ServerMessage::Frame(frame) => {
                            e.frame = Some(frame);
                            dirty |= id == selected;
                        }
                        ServerMessage::ServerShutdown { reason } => {
                            // Only detaching the selected server ends this client.
                            if id == selected && reason.as_deref() == Some("detached") {
                                break;
                            }
                            e.disconnect(reason.unwrap_or_else(|| "server stopped".into()));
                            dirty = true;
                        }
                        ServerMessage::Notify { kind, message } => {
                            e.unread = id != selected;
                            super::handle_notify(
                                kind,
                                &format!("{}: {}", e.label, clean(&message)),
                                &sound,
                            );
                            dirty = true;
                        }
                        ServerMessage::Clipboard { data } if id == selected => {
                            super::forward_clipboard(&data);
                        }
                        ServerMessage::MouseCapture { enabled } => {
                            e.mouse_capture = enabled;
                            dirty = true;
                        }
                        ServerMessage::ReloadSoundConfig => {
                            super::reload_local_client_config(&mut sound, &mut redraw_focus);
                        }
                        // Pop-out's local API cannot address a terminal on a different server.
                        ServerMessage::PopOutPane {
                            terminal_id,
                            title,
                            column,
                            row,
                            columns,
                            rows,
                        } if e.profile.is_none() => {
                            let sequence = crate::black_box::pop_out_sequence(
                                &terminal_id,
                                &title,
                                column,
                                row,
                                columns,
                                rows,
                            );
                            io::stdout().write_all(sequence.as_bytes())?;
                            io::stdout().flush()?;
                        }
                        ServerMessage::PopOutPane { .. } => {
                            super::handle_notify(
                                NotifyKind::Toast,
                                "Remote pop-out is unavailable; use a pane or tab in this machine.",
                                &sound,
                            );
                        }
                        _ => {}
                    }
                }
            }
            ClientLoopEvent::Timer => {}
            ClientLoopEvent::ServerMessage(_) | ClientLoopEvent::ServerDisconnected => {}
        }
    }
    for endpoint in &mut endpoints {
        endpoint.send(&ClientMessage::Detach);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoints() -> Vec<Endpoint> {
        let machine =
            Machine::new("VM 223".into(), "VM223".into(), "motionharvest".into()).unwrap();
        vec![
            Endpoint::local(),
            Endpoint::new(machine.id.clone(), machine.label.clone(), Some(machine)),
        ]
    }

    #[test]
    fn disabling_selected_machine_returns_to_local_and_drops_cached_surface() {
        let mut endpoints = endpoints();
        let mut selected = endpoints[1].id.clone();
        reconcile(&mut endpoints, &Catalog::default(), &mut selected);
        assert_eq!(selected, "local");
        assert_eq!(endpoints.len(), 1);
    }

    #[test]
    fn disconnected_surfaces_are_not_input_ready_and_render_error() {
        let mut endpoints = endpoints();
        endpoints[1].frame = Some(empty_frame(80, 23));
        endpoints[1].disconnect("connection lost".into());
        assert!(!endpoints[1].ready(80, 23));
        let frame = compose(&endpoints, &endpoints[1].id, 80, 24, true);
        assert_eq!(frame.height, 24);
        assert_eq!(frame.cells.len(), 80 * 24);
        assert!(frame
            .cells
            .iter()
            .map(|c| c.symbol.as_str())
            .collect::<String>()
            .contains("connection lost"));
    }

    #[test]
    fn clicks_on_machine_bar_never_reach_a_server() {
        let endpoints = endpoints();
        assert!(matches!(
            input_action(b"\x1b[<0;20;24M", &endpoints, "local", 80, 24, true),
            InputAction::Select(_)
        ));
        assert!(matches!(
            input_action(b"\x1b[<0;20;24m", &endpoints, "local", 80, 24, true),
            InputAction::Consume
        ));
        assert!(matches!(
            input_action(b"hello", &endpoints, "local", 80, 24, true),
            InputAction::Forward
        ));
        assert!(matches!(
            input_action(b"\x1b[<0;20;2M", &endpoints, "local", 80, 24, true),
            InputAction::Forward
        ));
        let mut router = InputRouter::default();
        assert!(matches!(
            router.action(b"\x1b[<0;20;24M", &endpoints, "local", (80, 24), true),
            InputAction::Select(_)
        ));
        // A release or drag off the bar must not land in the newly selected VM.
        assert!(matches!(
            router.action(b"\x1b[<32;20;2M", &endpoints, "local", (80, 24), true),
            InputAction::Consume
        ));
        assert!(matches!(
            router.action(b"\x1b[<0;20;2m", &endpoints, "local", (80, 24), true),
            InputAction::Consume
        ));
        assert!(matches!(
            router.action(b"\x1b[<0;20;2M", &endpoints, "local", (80, 24), true),
            InputAction::Forward
        ));
    }

    #[test]
    fn narrow_and_tiny_terminals_keep_valid_frames_and_selected_tab_visible() {
        let endpoints = endpoints();
        for cols in [1, 5, 20, 80] {
            for rows in [1, 2, 24] {
                let frame = compose(&endpoints, &endpoints[1].id, cols, rows, true);
                assert_eq!(frame.cells.len(), cols as usize * rows as usize);
                assert_eq!(frame.height, rows);
            }
            assert!(tabs(&endpoints, &endpoints[1].id, cols)
                .iter()
                .any(|(i, _, _)| *i == 1));
        }
    }

    #[test]
    fn input_is_routed_only_to_the_selected_machine_even_with_equal_terminal_ids() {
        let mut endpoints = endpoints();
        let (local, mut local_server) = UnixStream::pair().unwrap();
        let (remote, mut remote_server) = UnixStream::pair().unwrap();
        local_server
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        remote_server
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        endpoints[0].connection = Some(Connection {
            stream: local,
            tunnel: None,
        });
        endpoints[1].connection = Some(Connection {
            stream: remote,
            tunnel: None,
        });
        endpoints[0].frame = Some(empty_frame(80, 23));
        endpoints[1].frame = Some(empty_frame(80, 23));
        let selected = endpoints[1].id.clone();
        let msg = ClientMessage::Input {
            data: b"echo remote\n".to_vec(),
        };
        endpoints
            .iter_mut()
            .find(|e| e.id == selected)
            .unwrap()
            .send(&msg);
        assert_eq!(
            protocol::read_message::<_, ClientMessage>(&mut remote_server, MAX_FRAME_SIZE).unwrap(),
            msg
        );
        assert!(
            protocol::read_message::<_, ClientMessage>(&mut local_server, MAX_FRAME_SIZE).is_err()
        );
        drop(endpoints);
        // Drop shuts the sockets down so readers and bridge workers cannot leak.
        assert!(matches!(
            protocol::read_message::<_, ClientMessage>(&mut remote_server, MAX_FRAME_SIZE),
            Err(protocol::FramingError::UnexpectedEof)
        ));
    }

    #[test]
    fn server_reader_tags_frames_and_disconnects_with_connection_generation() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let mut endpoint = Endpoint::local();
        endpoint.generation = 7;
        endpoint.connection = Some(Connection {
            stream: client,
            tunnel: None,
        });
        let (tx, mut rx) = mpsc::channel(4);
        reader(&endpoint, &tx).unwrap();
        protocol::write_message(&mut server, &ServerMessage::Frame(empty_frame(2, 2))).unwrap();
        assert!(
            matches!(rx.blocking_recv(), Some(ClientLoopEvent::Machine(Event::Message { id, generation: 7, .. })) if id == "local")
        );
        drop(server);
        assert!(matches!(
            rx.blocking_recv(),
            Some(ClientLoopEvent::Machine(Event::Disconnected {
                generation: 7,
                ..
            }))
        ));
    }
}
