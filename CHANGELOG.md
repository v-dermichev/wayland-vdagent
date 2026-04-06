# Changelog

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
