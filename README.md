# wayland-vdagent

> **⚠ Archived — further work continues upstream.**
> A C port of this implementation has been drafted directly into the
> upstream SPICE Linux vd_agent tree, where ongoing development will
> continue: <https://gitlab.freedesktop.org/spice/linux/vd_agent>.
>
> This Rust prototype is left online as a behavioural reference and is
> no longer actively developed.

SPICE clipboard bridge for Wayland compositors. Replaces the X11-only `spice-vdagent` per-session agent with a Wayland-native implementation.

## What it does

Bridges a SPICE host and a Wayland guest VM:

- **Clipboard** — bidirectional, text and images (PNG, BMP, JPEG, TIFF)
- **Display resolution** — tracks `wl_output` and reports the live size; handles host→guest `MONITORS_CONFIG` resize requests via `hyprctl` / `swaymsg`
- **File transfer** — host→guest drag-and-drop into the viewer window; files land in `$XDG_DOWNLOAD_DIR`

Works with Hyprland, Sway, and any compositor supporting either `ext-data-control-v1` (stable) or `wlr-data-control-unstable-v1` (legacy). Ext is preferred when both are advertised.

## How it works

Connects to the existing `spice-vdagentd` daemon via its Unix socket. Uses lazy data serving (matching the Windows SPICE agent pattern) — clipboard data is only fetched when an application actually pastes, not when the host announces a clipboard change.

Resolution reporting is driven by `wl_output`: the agent binds the first output, tracks its current mode, and pushes `VDAGENTD_GUEST_XORG_RESOLUTION` on every commit so the SPICE host always sees the real guest size, including after a compositor-side resize.

## Requirements

- `spice-vdagentd` running as a system service
- A Wayland compositor exposing `ext-data-control-v1` or `zwlr_data_control_manager_v1`
- QEMU/KVM with SPICE display

## Compatibility

Works on any Wayland compositor exposing `ext-data-control-v1` (preferred) or `wlr-data-control-unstable-v1`:

| Compositor           | ext-data-control-v1 | wlr-data-control |
|----------------------|---------------------|------------------|
| Hyprland             | 0.52.1+             | earlier versions |
| Sway                 | 1.11+               | earlier versions |
| KWin (Plasma)        | 6.6+                | earlier versions |
| Mutter (GNOME)       | 49.2+               | —                |
| Weston               | 14.0.2+             | —                |
| river, wlroots-based | varies              | yes              |

### Tested

| Compositor | Version | Protocol            | Notes                          |
|------------|---------|---------------------|--------------------------------|
| Hyprland   | 0.54.3  | ext-data-control-v1 | Artix guest, QEMU/KVM + SPICE  |

## Install

Download the binary from [Releases](https://github.com/v-dermichev/wayland-vdagent/releases) or build from source:

```sh
cargo build --release
sudo cp target/release/wayland-vdagent /usr/local/bin/
```

## Setup

### Daemon (OpenRC)

```sh
# Create init script
cat << 'EOF' | sudo tee /etc/init.d/spice-vdagentd
#!/sbin/openrc-run
description="SPICE guest agent daemon"
command=/usr/bin/spice-vdagentd
command_args="-x"
command_background=true
pidfile=/run/spice-vdagentd.pid
start_pre() { mkdir -p /run/spice-vdagentd; }
EOF
sudo chmod +x /etc/init.d/spice-vdagentd
sudo rc-update add spice-vdagentd default
```

### Autostart (Hyprland)

```
exec-once = wayland-vdagent
```

### Autostart (Sway)

```
exec wayland-vdagent
```

### Autostart (systemd user service)

Recommended on systemd distros. A ready-to-use unit ships in [`contrib/wayland-vdagent.service`](contrib/wayland-vdagent.service):

```sh
install -Dm644 contrib/wayland-vdagent.service ~/.config/systemd/user/wayland-vdagent.service
systemctl --user enable --now wayland-vdagent.service
```

It's bound to `graphical-session.target`, so it starts after your compositor and stops with it.

### Autostart (compositor-agnostic)

Any compositor that honours the XDG autostart spec will pick this up. Drop the following at `~/.config/autostart/wayland-vdagent.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=wayland-vdagent
Exec=wayland-vdagent
OnlyShowIn=Wayland;
X-GNOME-Autostart-enabled=true
NoDisplay=true
```

## Build

```sh
cargo build --release
```

## License

MIT
