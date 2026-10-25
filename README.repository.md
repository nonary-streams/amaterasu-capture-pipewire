# Standalone Amaterasu PipeWire capture source proof

Extracted from Amaterasu commit `a6b0db9e36a1a59328ab4b4785b27b210f8182b7`. The original module README
is `README.md`. The existing `amaterasu-capture-pipewire` cdylib/rlib, C adapter
and KWin shim are retained. The optional `linux-kwin-screencast` feature now
reads the same pinned upstream Wayland protocol XML from this repository's own
`protocols/` directory; `protocols/README.md` records its upstream and license.

The existing module service identity and ABI export have not changed. The SDK
and media crates are pinned to local foundation commit `51ef23b1923335e0bb0c6af9eef685c6f4807520`.
No remote is configured. The `file://` Git URL is machine-local and must become
an approved stable source location before publication or cross-machine builds.

Default Linux native build needs PipeWire and GIO development headers. The
optional KWin feature additionally needs Wayland client headers and scanner and
must be qualified against the installed KWin protocol version. This source
extraction does not open a capture session or authorize portal, GPU, display,
network, or package changes. The host's signed manifest still authenticates
artifact bytes, caller grants and component identity before loading.
