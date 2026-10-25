# Standalone PipeWire capture source

This crate builds the Linux `amaterasu-capture-pipewire` native module. See
`README.md` for portal consent, frame lifetime and optional KWin limits.
`Cargo.toml` pins the public `nonary-streams/amaterasu-sdk` Git identity and
exact SDK commit; `Cargo.lock` records the locally validated resolution.
`cargo test --locked --offline --no-run` checks source without a portal or
capture device. The optional KWin feature needs Wayland scanner and headers.
The bundled protocol XML and its upstream notices remain under `protocols/`.
`zkde-screencast-unstable-v1.xml` is LGPL-2.1-or-later; the verbatim license
text is in `third-party/LGPL-2.1-or-later.txt`. The `xdg-output` XML includes
its MIT notice. Generated-code distributions must retain applicable notices.
No live portal or service was opened. Project-authored source is MIT licensed; the
bundled protocol licenses remain separate.
