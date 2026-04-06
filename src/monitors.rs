//! Host → Guest monitor configuration (response to `VDAGENTD_MONITORS_CONFIG`).
//!
//! The reference Linux agent's own comment (`display.c`) says:
//!
//! > FIXME: there is no equivalent call to set the monitor config under wayland
//!
//! We do better by dispatching to the running compositor's CLI — Hyprland via
//! `hyprctl`, Sway/river via `swaymsg`. When `wl_output::Mode` fires in response
//! to the applied change, the existing resolution-tracking path will push a
//! fresh `GUEST_XORG_RESOLUTION` back to the daemon.

use std::process::Command;

/// Parse the body of a `VDAGENTD_MONITORS_CONFIG` message.
///
/// Layout (little-endian, packed):
///
/// ```text
/// u32 num_of_monitors
/// u32 flags
/// [num] * VDAgentMonConfig {
///     u32 height    // note: height before width
///     u32 width
///     u32 depth
///     i32 x
///     i32 y
/// }
/// ```
///
/// Returns the `(width, height)` of the first monitor — single-output is the
/// common VM case and keeping it simple avoids guessing per-output mapping.
pub fn parse_first_monitor(data: &[u8]) -> Option<(u32, u32)> {
    const HEADER: usize = 8;
    const MON: usize = 20;
    if data.len() < HEADER + MON {
        return None;
    }
    let num = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if num == 0 {
        return None;
    }
    let height = u32::from_le_bytes(data[HEADER..HEADER + 4].try_into().unwrap());
    let width = u32::from_le_bytes(data[HEADER + 4..HEADER + 8].try_into().unwrap());
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// Try to apply a single-monitor resolution request by shelling out to the
/// running compositor. Returns `true` if the command was dispatched
/// successfully — the actual resize shows up asynchronously via `wl_output`.
pub fn apply(width: u32, height: u32) -> bool {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() && apply_hyprland(width, height) {
        return true;
    }
    if std::env::var_os("SWAYSOCK").is_some() && apply_sway(width, height) {
        return true;
    }
    eprintln!(
        "wayland-vdagent: MONITORS_CONFIG {width}x{height} — no supported compositor backend"
    );
    false
}

fn apply_hyprland(width: u32, height: u32) -> bool {
    // Empty monitor name → applies to the first / current output, which is
    // what SPICE guests have. Using @60 is fine for virtual outputs.
    let arg = format!(",{width}x{height}@60,0x0,1");
    match Command::new("hyprctl")
        .args(["keyword", "monitor", &arg])
        .status()
    {
        Ok(s) if s.success() => {
            eprintln!("wayland-vdagent: hyprctl set monitor to {width}x{height}");
            true
        }
        Ok(s) => {
            eprintln!("wayland-vdagent: hyprctl exited with {s}");
            false
        }
        Err(e) => {
            eprintln!("wayland-vdagent: failed to spawn hyprctl: {e}");
            false
        }
    }
}

fn apply_sway(width: u32, height: u32) -> bool {
    let mode = format!("{width}x{height}");
    // `*` is sway's wildcard — applies to every output.
    match Command::new("swaymsg")
        .args(["output", "*", "mode", &mode])
        .status()
    {
        Ok(s) if s.success() => {
            eprintln!("wayland-vdagent: swaymsg set outputs to {width}x{height}");
            true
        }
        Ok(s) => {
            eprintln!("wayland-vdagent: swaymsg exited with {s}");
            false
        }
        Err(e) => {
            eprintln!("wayland-vdagent: failed to spawn swaymsg: {e}");
            false
        }
    }
}
