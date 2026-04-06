//! SPICE clipboard bridge for Wayland.
//!
//! Matches the Windows SPICE agent flow: on host GRAB we only *offer* a data
//! source; on paste (compositor's `Send` event) we then issue REQUEST to the
//! daemon and forward the bytes. Eager REQUEST breaks the SPICE grab channel.

mod data_control;
mod udscs;

use data_control::*;
use std::io::{Read, Write};
use std::os::unix::io::{AsFd, AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use udscs::*;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, EventQueue, QueueHandle, WEnum,
};

const VDAGENTD_SOCKET: &str = "/run/spice-vdagentd/spice-vdagent-sock";

/// Maximum idle time while waiting for a clipboard payload from spice-vdagentd.
/// Matches the Windows agent's `VD_CLIPBOARD_TIMEOUT_MS`. Applied via
/// `set_read_timeout`, which is a *per-read* deadline — so the overall wait for
/// a multi-chunk payload (e.g. a large image) can last much longer than this,
/// as long as bytes keep arriving without a gap longer than `CLIPBOARD_IDLE_TIMEOUT`.
const CLIPBOARD_IDLE_TIMEOUT: Duration = Duration::from_millis(3000);

/// Short deadline used when opportunistically draining the daemon socket after
/// `poll()` has already told us data is pending. Just keeps a buggy daemon from
/// wedging the event loop if the socket signalled readable but returned nothing.
const DAEMON_PEEK_TIMEOUT: Duration = Duration::from_millis(10);

/// For each SPICE clipboard type we care about, the list of Wayland mime types
/// that map to it. The first entry in each list is the "canonical" mime — it's
/// what we use when receiving from a remote offer if we need to pick one.
struct ClipboardFormat {
    spice_type: u32,
    mimes: &'static [&'static str],
}

const FORMATS: &[ClipboardFormat] = &[
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
        mimes: &[
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "STRING",
            "TEXT",
        ],
    },
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_IMAGE_PNG,
        mimes: &["image/png"],
    },
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_IMAGE_BMP,
        mimes: &["image/bmp", "image/x-bmp"],
    },
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_IMAGE_JPG,
        mimes: &["image/jpeg", "image/jpg"],
    },
    ClipboardFormat {
        spice_type: VD_AGENT_CLIPBOARD_IMAGE_TIFF,
        mimes: &["image/tiff"],
    },
];

/// Given an advertised-mime list (from an incoming offer), return the SPICE
/// types we can serve. Keeps original `FORMATS` order so GRAB types are stable.
fn spice_types_for_mimes(mimes: &[String]) -> Vec<u32> {
    FORMATS
        .iter()
        .filter(|f| f.mimes.iter().any(|m| mimes.iter().any(|om| om == m)))
        .map(|f| f.spice_type)
        .collect()
}

/// Pick the canonical mime for a given advertised-mime list and a wanted SPICE
/// type. Returns `None` if the offer does not advertise any compatible mime.
fn canonical_mime_for(spice_type: u32, mimes: &[String]) -> Option<String> {
    let fmt = FORMATS.iter().find(|f| f.spice_type == spice_type)?;
    for candidate in fmt.mimes {
        if mimes.iter().any(|m| m == candidate) {
            return Some((*candidate).to_string());
        }
    }
    None
}

/// Map the mime the compositor asked for (in a source `Send` event) back to
/// its SPICE clipboard type.
fn spice_type_for_mime(mime: &str) -> Option<u32> {
    FORMATS
        .iter()
        .find(|f| f.mimes.contains(&mime))
        .map(|f| f.spice_type)
}

struct AppState {
    seat: Option<WlSeat>,
    manager: Option<Manager>,
    device: Option<Device>,
    daemon: Arc<Mutex<UnixStream>>,
    conn: Option<Connection>,
    current_source: Option<Source>,
    current_offer: Option<Offer>,
    // Mimes advertised by the current offer, accumulated from
    // `data_control_offer.offer` events before `Selection` arrives.
    current_offer_mimes: Vec<String>,
    we_own_clipboard: bool,
    // Globals discovered during first roundtrip; one is bound afterwards.
    ext_manager_name: Option<u32>,
    wlr_manager_name: Option<u32>,
    // Tracked output geometry. `width`/`height` are the last values we've
    // already reported to the daemon; `pending_*` are staged from wl_output
    // Mode events and committed on `done`.
    width: i32,
    height: i32,
    pending_width: i32,
    pending_height: i32,
}

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("wayland-vdagent {}", env!("GIT_VERSION"));
        return;
    }
    eprintln!("wayland-vdagent: starting");

    let stream = UnixStream::connect(VDAGENTD_SOCKET).unwrap_or_else(|e| {
        eprintln!("failed to connect to daemon: {}", e);
        std::process::exit(1);
    });
    eprintln!("wayland-vdagent: daemon connected");

    let daemon = Arc::new(Mutex::new(stream));

    let conn = Connection::connect_to_env().expect("failed to connect to Wayland");
    let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = AppState {
        seat: None,
        manager: None,
        device: None,
        daemon: daemon.clone(),
        conn: Some(conn.clone()),
        current_source: None,
        current_offer: None,
        current_offer_mimes: Vec::new(),
        we_own_clipboard: false,
        ext_manager_name: None,
        wlr_manager_name: None,
        width: 0,
        height: 0,
        pending_width: 0,
        pending_height: 0,
    };

    let registry = conn.display().get_registry(&qh, ());
    event_queue.roundtrip(&mut state).expect("roundtrip failed");

    // Now that all globals are known, bind exactly one data-control manager.
    state.manager = if let Some(name) = state.ext_manager_name {
        eprintln!("wayland-vdagent: using ext-data-control-v1");
        Some(Manager::Ext(
            registry.bind::<ExtDataControlManagerV1, _, _>(name, 1, &qh, ()),
        ))
    } else if let Some(name) = state.wlr_manager_name {
        eprintln!("wayland-vdagent: using wlr-data-control (legacy)");
        Some(Manager::Wlr(
            registry.bind::<ZwlrDataControlManagerV1, _, _>(name, 2, &qh, ()),
        ))
    } else {
        None
    };

    let (Some(manager), Some(seat)) = (&state.manager, &state.seat) else {
        eprintln!("wayland-vdagent: missing manager or seat");
        std::process::exit(1);
    };
    state.device = Some(manager.get_data_device(seat, &qh));

    // Second roundtrip so wl_output Mode/Done events arrive (they come after
    // the global advertisement we processed above).
    event_queue.roundtrip(&mut state).expect("roundtrip failed");

    if state.width == 0 || state.height == 0 {
        eprintln!("wayland-vdagent: no wl_output geometry yet, falling back to 1280x800");
        state.width = 1280;
        state.height = 800;
        send_resolution(&state.daemon, state.width, state.height);
    }
    eprintln!(
        "wayland-vdagent: Wayland ready ({}x{})",
        state.width, state.height
    );

    let daemon_fd = daemon.lock().unwrap().as_raw_fd();
    let wayland_fd = conn.as_fd().as_raw_fd();

    loop {
        conn.flush().ok();

        let mut fds = [
            PollFd {
                fd: daemon_fd,
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: wayland_fd,
                events: POLLIN,
                revents: 0,
            },
        ];
        unsafe { poll(fds.as_mut_ptr(), 2, -1) };

        if fds[0].revents & POLLIN != 0 {
            let mut d = daemon.lock().unwrap();
            d.set_read_timeout(Some(DAEMON_PEEK_TIMEOUT)).ok();
            match read_msg(&mut d) {
                Ok(msg) => {
                    drop(d);
                    handle_daemon_msg(&mut state, &qh, &msg);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    eprintln!("wayland-vdagent: daemon error: {}", e);
                    break;
                }
            }
        }

        if fds[1].revents & POLLIN != 0 {
            if let Some(guard) = conn.prepare_read() {
                guard.read().ok();
            }
            event_queue.dispatch_pending(&mut state).ok();
        }
    }
}

fn send_resolution(daemon: &Mutex<UnixStream>, width: i32, height: i32) {
    let d = daemon.lock().unwrap();
    let res = guest_xorg_resolution(width, height, 0, 0, 0);
    send_msg(
        &d,
        VDAGENTD_GUEST_XORG_RESOLUTION,
        width as u32,
        height as u32,
        &res,
    );
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
            eprintln!(
                "wayland-vdagent: graphics device info, resending resolution {}x{}",
                state.width, state.height
            );
            send_resolution(&state.daemon, state.width, state.height);
        }
        VDAGENTD_CLIPBOARD_GRAB => {
            // Only handle CLIPBOARD selection (not PRIMARY).
            if msg.arg1 != 0 {
                return;
            }
            // Collect the SPICE types advertised by the host and map each to
            // the full set of Wayland mimes we can offer for that type.
            let host_types: Vec<u32> = msg
                .data
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            let mimes_to_offer: Vec<&'static str> = FORMATS
                .iter()
                .filter(|f| host_types.contains(&f.spice_type))
                .flat_map(|f| f.mimes.iter().copied())
                .collect();
            if mimes_to_offer.is_empty() {
                eprintln!("wayland-vdagent: host GRAB with no supported types ({host_types:?})",);
                return;
            }

            eprintln!(
                "wayland-vdagent: host GRAB — offering {} mimes for types {host_types:?}",
                mimes_to_offer.len()
            );

            // Fresh source per grab — the previous one may already be cancelled.
            if let Some(manager) = &state.manager {
                if let Some(old) = state.current_source.take() {
                    old.destroy();
                }
                let source = manager.create_data_source(qh);
                for mime in &mimes_to_offer {
                    source.offer((*mime).to_string());
                }
                if let Some(device) = &state.device {
                    device.set_selection(Some(&source));
                }
                state.current_source = Some(source);
                state.we_own_clipboard = true;
            }
        }
        VDAGENTD_CLIPBOARD_DATA => {
            // CLIPBOARD_DATA that arrives through the main loop is stray — the
            // real response to our REQUEST is drained synchronously inside
            // `handle_source_send`. Drop it.
        }
        VDAGENTD_CLIPBOARD_REQUEST => {
            let wanted_type = msg.arg2;
            eprintln!("wayland-vdagent: host requests our clipboard (type={wanted_type})");
            let data = state.current_offer.as_ref().and_then(|offer| {
                canonical_mime_for(wanted_type, &state.current_offer_mimes)
                    .and_then(|mime| read_offer(offer, &mime, state.conn.as_ref()))
            });
            let d = state.daemon.lock().unwrap();
            match data {
                Some(bytes) if !bytes.is_empty() => {
                    eprintln!(
                        "wayland-vdagent: sending {} bytes to host (type={wanted_type})",
                        bytes.len()
                    );
                    send_msg(&d, VDAGENTD_CLIPBOARD_DATA, msg.arg1, wanted_type, &bytes);
                }
                _ => {
                    eprintln!("wayland-vdagent: no data from offer");
                    send_msg(
                        &d,
                        VDAGENTD_CLIPBOARD_DATA,
                        msg.arg1,
                        VD_AGENT_CLIPBOARD_NONE,
                        &[],
                    );
                }
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
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, 1, qh, ()));
                }
                "wl_output" => {
                    // We only care about the first output — SPICE reports one.
                    // Binding additional outputs is harmless (dispatch is a noop
                    // if we already have geometry), but avoid leaking extras.
                    let _ = registry.bind::<WlOutput, _, _>(name, 2, qh, ());
                }
                EXT_MANAGER_INTERFACE => state.ext_manager_name = Some(name),
                WLR_MANAGER_INTERFACE => state.wlr_manager_name = Some(name),
                _ => {}
            }
        }
    }
}

delegate_noop!(AppState: ignore WlSeat);
delegate_noop!(AppState: ignore ZwlrDataControlManagerV1);
delegate_noop!(AppState: ignore ExtDataControlManagerV1);

impl Dispatch<ZwlrDataControlOfferV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            handle_offer_mime(state, mime_type);
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            handle_offer_mime(state, mime_type);
        }
    }
}

impl Dispatch<WlOutput, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                // Only the current mode is interesting; compositors also advertise
                // non-current modes when enumerating the output's capabilities.
                let is_current = match flags {
                    WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                    _ => false,
                };
                if is_current {
                    state.pending_width = width;
                    state.pending_height = height;
                }
            }
            wl_output::Event::Done => {
                if state.pending_width > 0
                    && state.pending_height > 0
                    && (state.pending_width != state.width || state.pending_height != state.height)
                {
                    state.width = state.pending_width;
                    state.height = state.pending_height;
                    eprintln!(
                        "wayland-vdagent: output resized to {}x{}",
                        state.width, state.height
                    );
                    send_resolution(&state.daemon, state.width, state.height);
                }
            }
            _ => {}
        }
    }
}

fn handle_data_offer(state: &mut AppState, offer: Offer) {
    if let Some(old) = state.current_offer.take() {
        old.destroy();
    }
    state.current_offer = Some(offer);
    state.current_offer_mimes.clear();
}

fn handle_offer_mime(state: &mut AppState, mime_type: String) {
    state.current_offer_mimes.push(mime_type);
}

fn handle_selection(state: &mut AppState, has_offer: bool) {
    if !has_offer || state.we_own_clipboard {
        return;
    }
    let types = spice_types_for_mimes(&state.current_offer_mimes);
    if types.is_empty() {
        eprintln!(
            "wayland-vdagent: guest clipboard changed but no supported mime ({:?})",
            state.current_offer_mimes
        );
        return;
    }
    eprintln!("wayland-vdagent: guest clipboard changed, sending GRAB for types {types:?}");
    let mut payload = Vec::with_capacity(types.len() * 4);
    for t in &types {
        payload.extend_from_slice(&t.to_le_bytes());
    }
    let d = state.daemon.lock().unwrap();
    send_msg(&d, VDAGENTD_CLIPBOARD_GRAB, 0, 0, &payload);
}

/// Drain `offer` into a `Vec` via a pipe for a given mime type.
/// Returns `None` if pipe creation fails.
fn read_offer(offer: &Offer, mime: &str, conn: Option<&Connection>) -> Option<Vec<u8>> {
    let mut fds = [0i32; 2];
    if unsafe { libc_pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let write_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
    offer.receive(mime.to_string(), write_fd.as_fd());
    // Flush so the compositor actually sees the receive request, then close
    // our write end so the subsequent read terminates with EOF.
    if let Some(c) = conn {
        c.flush().ok();
    }
    drop(write_fd);

    let mut read_file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut data = Vec::new();
    let _ = Read::read_to_end(&mut read_file, &mut data);
    Some(data)
}

fn handle_source_send(state: &mut AppState, mime_type: String, fd: std::os::fd::OwnedFd) {
    let Some(spice_type) = spice_type_for_mime(&mime_type) else {
        eprintln!("wayland-vdagent: paste request for unsupported mime {mime_type}");
        return;
    };
    eprintln!("wayland-vdagent: paste request for {mime_type} (type={spice_type})");

    let mut d = state.daemon.lock().unwrap();
    send_msg(&d, VDAGENTD_CLIPBOARD_REQUEST, 0, spice_type, &[]);

    // Per-read idle timeout — each successful read resets it, so a large
    // image that arrives in many chunks is fine as long as chunks keep coming.
    d.set_read_timeout(Some(CLIPBOARD_IDLE_TIMEOUT)).ok();
    let mut data = Vec::new();
    loop {
        match read_msg(&mut d) {
            Ok(msg) if msg.msg_type == VDAGENTD_CLIPBOARD_DATA && msg.arg1 == 0 => {
                data = msg.data;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    d.set_read_timeout(Some(DAEMON_PEEK_TIMEOUT)).ok();
    drop(d);

    if !data.is_empty() {
        eprintln!(
            "wayland-vdagent: serving {} bytes ({mime_type})",
            data.len()
        );
        let raw_fd = IntoRawFd::into_raw_fd(fd);
        let mut file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
        let _ = file.write_all(&data);
    } else {
        eprintln!("wayland-vdagent: no data from SPICE for {mime_type}");
    }
}

fn handle_source_cancelled(state: &mut AppState) {
    eprintln!("wayland-vdagent: clipboard source cancelled");
    state.we_own_clipboard = false;
}

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
                handle_data_offer(state, Offer::Wlr(id))
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                handle_selection(state, id.is_some())
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                handle_source_send(state, mime_type, fd)
            }
            zwlr_data_control_source_v1::Event::Cancelled => handle_source_cancelled(state),
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for AppState {
    event_created_child!(AppState, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ()),
    ]);
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                handle_data_offer(state, Offer::Ext(id))
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                handle_selection(state, id.is_some())
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                handle_source_send(state, mime_type, fd)
            }
            ext_data_control_source_v1::Event::Cancelled => handle_source_cancelled(state),
            _ => {}
        }
    }
}

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
