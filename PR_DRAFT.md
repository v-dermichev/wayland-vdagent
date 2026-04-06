# Proposal: Wayland clipboard support for SPICE guest agent

## Target

- **Issue**: https://gitlab.freedesktop.org/spice/linux/vd_agent/-/issues/26
- **Project**: https://gitlab.freedesktop.org/spice/linux/vd_agent
- **Mailing list**: spice-devel@lists.freedesktop.org

## Title

Wayland clipboard support via wlr-data-control protocol (standalone agent)

## Proposal text

### Problem

The current `spice-vdagent` per-session agent relies on X11 (via `vdagent_x11.c`) or GTK3 with X11 backend (`clipboard.c` with `USE_GTK_FOR_CLIPBOARD`) for clipboard sharing. Neither path works under native Wayland compositors — clipboard sharing is completely broken when the guest runs Hyprland, Sway, or any wlroots-based compositor without XWayland.

This has been an open issue since 2022 (#26), and with Wayland adoption accelerating (Fedora, Ubuntu defaulting to Wayland, tiling compositor popularity), the gap is growing.

### Solution

I've implemented a standalone Wayland-native replacement for the per-session `spice-vdagent` process. It connects to the existing `spice-vdagentd` daemon via the standard Unix socket protocol (`udscs`) — no daemon changes needed.

**Key design decision: lazy data serving (matching the Windows agent)**

The critical insight came from studying `vd-agent-win32`. The Windows agent uses delayed rendering (`SetClipboardData(format, NULL)`) — it never requests clipboard data on GRAB. Data is only fetched when an application actually pastes (`WM_RENDERFORMAT`). This is essential because the SPICE client stops sending subsequent GRABs if the agent sends CLIPBOARD_REQUEST eagerly.

The Wayland implementation mirrors this pattern:

1. **On host GRAB**: Create a `zwlr_data_control_source_v1`, offer text MIME types, set as clipboard selection. No data request sent.
2. **On guest paste** (`Send` event from compositor): Send `CLIPBOARD_REQUEST` to daemon, block until `CLIPBOARD_DATA` arrives (up to 3s timeout, matching Windows), write data to compositor-provided fd.
3. **On guest copy** (`Selection` event with new offer): Send `CLIPBOARD_GRAB` to daemon. On subsequent `CLIPBOARD_REQUEST` from daemon, read from the offer via `zwlr_data_control_offer_v1::receive`.
4. **On client disconnect**: Release all clipboard selections (matching `vdagent_clipboards_release_all`).

### Architecture

```
┌─────────────────────────────────────────────────┐
│ Host (SPICE client / virt-manager)              │
└──────────────────┬──────────────────────────────┘
                   │ SPICE protocol
┌───────────��──────┴──────────────────────────────┐
│ QEMU SPICE server                               │
└──────────────────┬──────────────────────────────┘
                   │ virtio-serial (com.redhat.spice.0)
┌──────────────────┴──────────────────────────────┐
│ spice-vdagentd (UNCHANGED)                      │
│ - Handles virtio port, capabilities, mouse      │
│ - Forwards clipboard messages to session agent  │
└──────────────────┬──────────────────────────────┘
                   │ Unix socket (udscs protocol)
┌──────────────────┴──────────────────────────────┐
│ wayland-vdagent (NEW - replaces spice-vdagent)  │
│ - wlr-data-control for clipboard                │
│ - Lazy data serving (no eager REQUEST)          │
│ - Bidirectional: host↔guest                     │
└──────────────────┬──────────────────────────────┘
                   │ wlr-data-control protocol
┌──────────────────┴──────────────────────────────┐
│ Wayland compositor (Hyprland, Sway, etc.)       │
└─────────────────────────────────────────────────┘
```

### What works

- **Host → Guest**: Clipboard changes on host arrive in guest on paste (lazy, not eager)
- **Guest → Host**: Clipboard changes in guest forwarded to host via SPICE
- **SPICE GL mode**: Works with `listen type='none'` + GL rendering (no blinking/tearing)
- **wlroots compositors**: Tested on Hyprland, should work on Sway and any compositor implementing `wlr-data-control-unstable-v1`

### Implementation details

- Written in Rust (~430 lines), depends only on `wayland-client` and `wayland-protocols-wlr`
- 520KB static binary, libc-only runtime dependencies
- Connects to `spice-vdagentd` via existing `/run/spice-vdagentd/spice-vdagent-sock`
- Sends `GUEST_XORG_RESOLUTION` on connect (required for daemon to open virtio channel)
- Handles `VERSION`, `GRAPHICS_DEVICE_INFO`, `CLIPBOARD_GRAB/REQUEST/DATA/RELEASE`, `CLIENT_DISCONNECTED`
- Uses `event_created_child` for `DataOffer` objects from compositor
- Proper `prepare_read` + `dispatch_pending` Wayland event loop

### Possible paths forward

1. **Standalone tool** — users install `wayland-vdagent` alongside `spice-vdagent` package, use it instead of the X11 agent on Wayland sessions
2. **C port into spice-vdagent** — add a `vdagent_wayland.c` backend alongside existing `vdagent_x11.c`, auto-detect session type at startup
3. **GTK4 path** — GTK4 has native Wayland clipboard support; the existing `USE_GTK_FOR_CLIPBOARD` path could work if built with GTK4 (note: spice-vdagent already checks `GTK_CHECK_VERSION(3, 98, 0)` for GTK4 in `main()`)

### Repository

https://github.com/v-dermichev/wayland-vdagent

### Testing

Tested on:
- Artix Linux (OpenRC) with Hyprland compositor
- QEMU/KVM via libvirt/virt-manager
- SPICE with GL rendering (`listen type='none'`, virtio-gpu with accel3d)
- SPICE with TCP (`listen type='address'`, QXL/virtio-gpu without GL)
- spice-vdagent 0.23.0, QEMU 10.2.2, Ventoy 1.1.10
