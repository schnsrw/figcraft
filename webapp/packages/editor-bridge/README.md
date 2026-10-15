# FigCraft editor bridge

This package defines the versioned browser-shell contract around the Rust/WASM editor. It does not
implement an editor, document model, command registry, undo stack, conflict resolver or realtime
protocol.

The initial contract makes Rust/WASM the only owner of the collaboration WebSocket. The browser
shell obtains a short-lived, document-scoped ticket over the authenticated HTTP API and passes its
value and the `wss:` endpoint while opening a document. Rust uses the browser WebSocket API through
its existing `web-sys` transport.

Snapshots, operation batches, selections and export results are represented only as branded opaque
binary envelopes. Browser-shell code may store or forward an envelope, but must not interpret or
mutate its bytes.

## Compatibility

- Both sides advertise `EDITOR_BRIDGE_PROTOCOL_VERSION` during mount.
- A mismatched major protocol version fails before a document is opened.
- Additive event fields do not require a version increase; consumers must ignore fields they do
  not use.
- Removing or changing a field, event kind or method requires a new protocol version and a
  compatibility window for the previously supported version.
- The Rust adapter will implement this contract in the browser-integration milestone. Until then,
  this package is the reviewed boundary that implementation must satisfy.
