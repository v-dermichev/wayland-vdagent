# Changelog

## v0.3.1 (2026-04-06)

- Report the real display resolution instead of a hard-coded 1280x800
- Bind `wl_output` and track its current mode; re-send `VDAGENTD_GUEST_XORG_RESOLUTION` whenever the compositor commits a new size
- Still falls back to 1280x800 if no `wl_output` geometry is available by the time the second roundtrip finishes (unlikely in practice)
- Remove stale `PR_DRAFT.md` (upstream proposal now tracked at spice/linux/vd_agent!57)

## v0.3.0 (2026-04-06)

- Support `ext-data-control-v1` (stable) in addition to `wlr-data-control-unstable-v1`
- Prefer `ext-data-control-v1` when both protocols are advertised by the compositor
- Defer manager binding until after the initial registry roundtrip (no more leaked wlr binding when ext is also present)
- Ship systemd user unit in `contrib/wayland-vdagent.service`
- Document systemd user-service and XDG autostart paths in README
- Cleanup: event-driven main loop (no 50 ms wakeups), dead fields removed, helpers extracted, `delegate_noop!` for offer dispatchers

## v0.2.2 (2026-04-06)

- Add `--version` flag with git-based version info
- Add CI/CD: lint+build on push, binary release on tag
- Fix formatting (cargo fmt)
- Add proposal draft for SPICE upstream

## v0.2.1 (2026-04-06)

- Fix resolution reporting (match actual display size)
- Fix VM→Host clipboard (pipe-based offer reading with Wayland flush)
- Add `event_created_child` for DataOffer objects

## v0.2.0 (2026-04-06)

- Complete rewrite using `wlr-data-control` protocol
- Lazy data serving matching Windows SPICE agent (`SetClipboardData(NULL)`)
- Host→VM: offer clipboard on GRAB, serve data on paste (Send event)
- VM→Host: detect Selection change, send GRAB, read offer on REQUEST
- Works with SPICE GL rendering (`listen type=none`)
- ~500KB binary, libc-only runtime dependencies

## v0.1.0 (2026-04-05)

- Initial implementation using `wl-clipboard-rs`
- Connected to `spice-vdagentd` via Unix socket (udscs protocol)
- Identified key issues: eager REQUEST kills SPICE grab flow
- Discovered Windows agent lazy rendering pattern from source analysis
