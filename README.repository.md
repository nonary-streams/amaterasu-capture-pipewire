# Standalone PipeWire capture source (publication candidate)

This crate builds the Linux `amaterasu-capture-pipewire` native module. See
`README.md` for portal consent, frame lifetime and optional KWin limits.
`Cargo.toml` pins the proposed `nonary-streams/amaterasu-sdk` Git identity and
exact SDK commit; `Cargo.lock` records the locally validated resolution.
`cargo test --locked --offline --no-run` checks source without a portal or
capture device. The optional KWin feature needs Wayland scanner and headers.
The bundled protocol XML and its upstream notices remain under `protocols/`;
any generated-code distribution must retain the applicable notices. No live
portal or service was opened. The Amaterasu source license is pending.
