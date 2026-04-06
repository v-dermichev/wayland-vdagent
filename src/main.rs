// wayland-vdagent v2: SPICE clipboard bridge for Wayland
// Uses wlr-data-control protocol for lazy clipboard serving.
// Matches Windows SPICE agent: GRAB → offer clipboard, paste → REQUEST → serve data.

mod udscs;

use std::io::Read;
use std::io::Write;
use std::os::unix::io::{AsFd, AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use udscs::*;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

const VDAGENTD_SOCKET: &str = "/run/spice-vdagentd/spice-vdagent-sock";

// Shared state between Wayland events and SPICE daemon
struct AppState {
    seat: Option<WlSeat>,
    manager: Option<ZwlrDataControlManagerV1>,
    device: Option<ZwlrDataControlDeviceV1>,
    daemon: Arc<Mutex<UnixStream>>,
    conn: Option<Connection>,
    // Current source we set on the clipboard (for host→VM)
    current_source: Option<ZwlrDataControlSourceV1>,
    // Whether we own the clipboard (we set it from SPICE data)
    we_own_clipboard: bool,
    // Latest offer from another app (for VM→host)
    current_offer: Option<ZwlrDataControlOfferV1>,
    // Whether guest app owns clipboard
    guest_owns_clipboard: bool,
}

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("wayland-vdagent {}", env!("GIT_VERSION"));
        return;
    }
    eprintln!("wayland-vdagent: starting");

    // Connect to daemon
    let stream = match UnixStream::connect(VDAGENTD_SOCKET) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to connect to daemon: {}", e);
            std::process::exit(1);
        }
    };
    eprintln!("wayland-vdagent: daemon connected");

    let daemon = Arc::new(Mutex::new(stream));

    // Send initial resolution
    {
        let d = daemon.lock().unwrap();
        let res = guest_xorg_resolution(1280, 800, 0, 0, 0);
        send_msg(&d, VDAGENTD_GUEST_XORG_RESOLUTION, 1280, 800, &res);
    }

    // Connect to Wayland
    let conn = Connection::connect_to_env().expect("failed to connect to Wayland");
    let display = conn.display();

    let mut state = AppState {
        seat: None,
        manager: None,
        device: None,
        daemon: daemon.clone(),
        conn: None,
        current_source: None,
        we_own_clipboard: false,
        current_offer: None,
        guest_owns_clipboard: false,
    };

    state.conn = Some(conn.clone());

    let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
    let qh = event_queue.handle();

    display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state).expect("roundtrip failed");

    // Create data control device
    if let (Some(manager), Some(seat)) = (&state.manager, &state.seat) {
        let device = manager.get_data_device(seat, &qh, ());
        state.device = Some(device);
    } else {
        eprintln!("wayland-vdagent: missing manager or seat");
        std::process::exit(1);
    }

    eprintln!("wayland-vdagent: Wayland ready");

    // Set daemon socket to non-blocking for the main loop
    let daemon_stream = daemon.lock().unwrap();
    let daemon_fd = daemon_stream.as_raw_fd();
    drop(daemon_stream);

    // Main event loop: poll both Wayland and daemon
    loop {
        // Read and dispatch Wayland events
        if let Some(guard) = conn.prepare_read() {
            guard.read().ok();
        }
        event_queue.dispatch_pending(&mut state).ok();
        conn.flush().ok();

        // Poll daemon for messages (non-blocking, 50ms timeout)
        let mut fds = [
            PollFd {
                fd: daemon_fd,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: conn.as_fd().as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
        ];
        unsafe { poll(fds.as_mut_ptr(), 2, 50) };

        // Read daemon messages
        if fds[0].revents & POLLIN != 0 {
            let mut d = daemon.lock().unwrap();
            d.set_read_timeout(Some(Duration::from_millis(10))).ok();
            match read_msg(&mut *d) {
                Ok(msg) => {
                    drop(d);
                    handle_daemon_msg(&mut state, &qh, &msg);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    eprintln!("wayland-vdagent: daemon error: {}", e);
                    break;
                }
            }
        }

        // Process Wayland events from socket
        if fds[1].revents & POLLIN != 0 {
            if let Some(guard) = conn.prepare_read() {
                guard.read().ok();
            }
            event_queue.dispatch_pending(&mut state).ok();
            conn.flush().ok();
        }
    }
}

fn handle_daemon_msg(state: &mut AppState, qh: &QueueHandle<AppState>, msg: &UdscsMsg) {
    match msg.msg_type {
        VDAGENTD_VERSION => {
            let ver = String::from_utf8_lossy(&msg.data);
            eprintln!(
                "wayland-vdagent: daemon version: {}",
                ver.trim_end_matches('\0')
            );
        }
        VDAGENTD_GRAPHICS_DEVICE_INFO => {
            eprintln!("wayland-vdagent: graphics device info, sending resolution");
            let d = state.daemon.lock().unwrap();
            let res = guest_xorg_resolution(1280, 800, 0, 0, 0);
            send_msg(&d, VDAGENTD_GUEST_XORG_RESOLUTION, 1280, 800, &res);
        }
        VDAGENTD_CLIPBOARD_GRAB => {
            let sel_id = msg.arg1;
            if sel_id != 0 {
                return;
            } // only handle CLIPBOARD

            // Parse types
            let n_types = msg.data.len() / 4;
            let mut has_text = false;
            for i in 0..n_types {
                let t = u32::from_le_bytes(msg.data[i * 4..(i + 1) * 4].try_into().unwrap());
                if t == VD_AGENT_CLIPBOARD_UTF8_TEXT {
                    has_text = true;
                }
            }
            if !has_text {
                return;
            }

            eprintln!("wayland-vdagent: host GRAB — offering clipboard");

            // Create new source for each grab — old source may have been cancelled
            if let Some(manager) = &state.manager {
                if let Some(old) = state.current_source.take() {
                    old.destroy();
                }

                let source = manager.create_data_source(qh, ());
                source.offer("text/plain".to_string());
                source.offer("text/plain;charset=utf-8".to_string());
                source.offer("UTF8_STRING".to_string());
                source.offer("STRING".to_string());
                source.offer("TEXT".to_string());

                if let Some(device) = &state.device {
                    device.set_selection(Some(&source));
                }

                state.current_source = Some(source);
                state.we_own_clipboard = true;
            }
        }
        VDAGENTD_CLIPBOARD_DATA => {
            // Data arrives after we REQUEST it (in the source Send handler)
            // This is handled in the ZwlrDataControlSourceV1 dispatch
            // We need to pass data to the waiting Send handler
            // For now, this is handled via the shared daemon stream
        }
        VDAGENTD_CLIPBOARD_REQUEST => {
            eprintln!("wayland-vdagent: host requests our clipboard");
            if let Some(offer) = &state.current_offer {
                // Create pipe: compositor writes to fds[1], we read from fds[0]
                let mut fds = [0i32; 2];
                if unsafe { libc_pipe(fds.as_mut_ptr()) } == 0 {
                    let write_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
                    offer.receive("text/plain;charset=utf-8".to_string(), write_fd.as_fd());

                    // Flush so compositor gets the receive request
                    if let Some(c) = &state.conn {
                        c.flush().ok();
                    }
                    // Close write end so read gets EOF
                    drop(write_fd);

                    // Read data from compositor
                    let mut read_file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
                    let mut data = Vec::new();
                    let _ = Read::read_to_end(&mut read_file, &mut data);

                    if !data.is_empty() {
                        eprintln!("wayland-vdagent: sending {} bytes to host", data.len());
                        let d = state.daemon.lock().unwrap();
                        send_msg(
                            &d,
                            VDAGENTD_CLIPBOARD_DATA,
                            msg.arg1,
                            VD_AGENT_CLIPBOARD_UTF8_TEXT,
                            &data,
                        );
                    } else {
                        eprintln!("wayland-vdagent: no data from offer");
                        let d = state.daemon.lock().unwrap();
                        send_msg(
                            &d,
                            VDAGENTD_CLIPBOARD_DATA,
                            msg.arg1,
                            VD_AGENT_CLIPBOARD_NONE,
                            &[],
                        );
                    }
                }
            } else {
                eprintln!("wayland-vdagent: no offer available");
                let d = state.daemon.lock().unwrap();
                send_msg(
                    &d,
                    VDAGENTD_CLIPBOARD_DATA,
                    msg.arg1,
                    VD_AGENT_CLIPBOARD_NONE,
                    &[],
                );
            }
        }
        VDAGENTD_CLIPBOARD_RELEASE => {
            if msg.arg1 == 0 {
                state.we_own_clipboard = false;
                if let Some(source) = state.current_source.take() {
                    source.destroy();
                }
            }
        }
        VDAGENTD_CLIENT_DISCONNECTED => {
            eprintln!("wayland-vdagent: client disconnected");
            state.we_own_clipboard = false;
            if let Some(source) = state.current_source.take() {
                source.destroy();
            }
        }
        VDAGENTD_AUDIO_VOLUME_SYNC | VDAGENTD_MONITORS_CONFIG => {}
        other => {
            eprintln!("wayland-vdagent: unhandled msg type={}", other);
        }
    }
}

// Wayland dispatchers

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_seat" => {
                    let seat = registry.bind::<WlSeat, _, _>(name, 1, qh, ());
                    state.seat = Some(seat);
                }
                "zwlr_data_control_manager_v1" => {
                    let manager = registry.bind::<ZwlrDataControlManagerV1, _, _>(name, 2, qh, ());
                    state.manager = Some(manager);
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(AppState: ignore WlSeat);
delegate_noop!(AppState: ignore ZwlrDataControlManagerV1);

impl Dispatch<ZwlrDataControlOfferV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        _: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// Data control device — handles clipboard ownership changes
impl Dispatch<ZwlrDataControlDeviceV1, ()> for AppState {
    event_created_child!(AppState, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                // Store the offer — we'll need it if guest copies something
                if let Some(old) = state.current_offer.take() {
                    old.destroy();
                }
                state.current_offer = Some(id);
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if id.is_some() && !state.we_own_clipboard {
                    // A guest app copied something — send GRAB to daemon
                    eprintln!("wayland-vdagent: guest clipboard changed, sending GRAB");
                    state.guest_owns_clipboard = true;
                    let d = state.daemon.lock().unwrap();
                    let types = VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes();
                    send_msg(&d, VDAGENTD_CLIPBOARD_GRAB, 0, 0, &types);
                } else if id.is_none() {
                    state.guest_owns_clipboard = false;
                }
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id: _ } => {}
            zwlr_data_control_device_v1::Event::Finished => {}
            _ => {}
        }
    }
}

// Data control source — this is our clipboard source
// When a guest app pastes, the compositor sends Send { mime_type, fd }
// We REQUEST data from SPICE, wait for it, write to fd
impl Dispatch<ZwlrDataControlSourceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                eprintln!("wayland-vdagent: paste request for {}", mime_type);

                // We're inside Wayland dispatch — main loop is paused.
                // Safe to use the daemon stream directly (no concurrent reads).
                let mut d = state.daemon.lock().unwrap();

                // Send REQUEST — matches Windows WM_RENDERFORMAT
                send_msg(
                    &d,
                    VDAGENTD_CLIPBOARD_REQUEST,
                    0,
                    VD_AGENT_CLIPBOARD_UTF8_TEXT,
                    &[],
                );

                // Wait for CLIPBOARD_DATA (up to 3s like Windows agent)
                d.set_read_timeout(Some(Duration::from_secs(3))).ok();
                let mut data = Vec::new();
                loop {
                    match read_msg(&mut *d) {
                        Ok(msg) if msg.msg_type == VDAGENTD_CLIPBOARD_DATA && msg.arg1 == 0 => {
                            data = msg.data;
                            break;
                        }
                        Ok(_) => continue, // skip other messages
                        Err(_) => break,   // timeout
                    }
                }
                // Restore non-blocking for main loop
                d.set_read_timeout(Some(Duration::from_millis(10))).ok();
                drop(d);

                if !data.is_empty() {
                    eprintln!("wayland-vdagent: serving {} bytes", data.len());
                    let raw_fd = IntoRawFd::into_raw_fd(fd);
                    let mut file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
                    let _ = file.write_all(&data);
                } else {
                    eprintln!("wayland-vdagent: no data from SPICE");
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                eprintln!("wayland-vdagent: clipboard source cancelled");
                state.we_own_clipboard = false;
            }
            _ => {}
        }
    }
}

// Helpers

fn guest_xorg_resolution(w: i32, h: i32, x: i32, y: i32, id: i32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(20);
    buf.extend_from_slice(&w.to_le_bytes());
    buf.extend_from_slice(&h.to_le_bytes());
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf.extend_from_slice(&id.to_le_bytes());
    buf
}

const POLLIN: i16 = 1;
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
extern "C" {
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    #[link_name = "pipe"]
    fn libc_pipe(fds: *mut i32) -> i32;
}
