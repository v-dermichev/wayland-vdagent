# wayland-vdagent

SPICE clipboard bridge for Wayland compositors. Replaces the X11-only `spice-vdagent` per-session agent with a Wayland-native implementation.

## What it does

Enables bidirectional clipboard sharing between a SPICE host and a Wayland guest VM. Works with Hyprland, Sway, and any compositor supporting the `wlr-data-control` protocol.

## How it works

Connects to the existing `spice-vdagentd` daemon via its Unix socket. Uses lazy data serving (matching the Windows SPICE agent pattern) — clipboard data is only fetched when an application actually pastes, not when the host announces a clipboard change.

## Requirements

- `spice-vdagentd` running as a system service
- A Wayland compositor with `wlr-data-control-unstable-v1` support
- QEMU/KVM with SPICE display

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

## Build

```sh
cargo build --release
```

## License

MIT
