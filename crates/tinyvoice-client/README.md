# tinyvoice-client

`tinyvoice-client` is the typed host adapter for the loadable TinyVoice module.
It depends on `tinyvoice-bus` for the wire contract and TinyBus SDK for module
calls. It does not depend on the host-agnostic `tinyvoice` implementation or
the loadable `tinyvoice-module` crate.

Construct it with `TinyVoiceClient::new(module_client)` using the host's one
configured `ModuleClient`. Stateless calls continue through that configured
client and preserve each contract member's positional tuple and typed reply.
Stateful owners call TinyBus 0.3.2's ordinary `ModuleClient::bind` once at
creation and retain the resulting private peer-pinned client for later calls.
This is ordinary bus routing: it pins the current unique peer through alias
handover, but does not load an artifact proof or make an attestation claim.

`VadSession`, `RecorderSession`, `OwnedCaptureStream`, and
`OwnedHotkeySession` are explicit async leases. Their opaque resource handles
stay private. Closing an owner calls the corresponding module method on its
pinned peer; dropping a Rust handle does not imply remote cleanup. The existing
flat capture and hotkey methods remain compatibility calls and do not pin a
resource owner. OpenHuman keeps host policy such as volume clamping, microphone
permission, hotkey policy, and conversion from persisted configuration into
`VadConfig` in its own adapter.

`reserve_capture_owned` returns a pinned reservation before `CAPTURE_START`.
Keep that owner while calling `start`; after a timeout or cancellation, its
explicit `close` targets the creating peer and uses the module's existing
pending-start cancellation path. A successful start returns an
`OwnedCaptureStream` for polling and explicit stop. A reservation that never
starts can be released explicitly with the same owner's `close`. If the
reservation reply is lost before the caller learns its handle, the module's
existing expiry still bounds that unknown reservation. The flat
`reserve_capture`, `start_capture`, `poll_capture`, and `stop_capture` methods
remain available for compatibility; they use whichever peer the supplied
ordinary client addresses at each call.

The client keeps the configured `ModuleClient` deadline and reporting sink.
It does not add a second timeout. `VoiceCallError::kind()` exposes the closed
failure category; its `Debug` and `Display` omit remote prose. The explicit
`raw_message()` and `raw_wire_name()` accessors return untrusted remote values
and require a caller-owned redaction policy before display or logging.

The recording-broker tests use only TinyBus's in-memory transport. They record
actual service invocations and verify wire arguments, returned types, peer
handover, and owner-close behavior; they do not load a published artifact,
access a microphone, or contact a backend.

The client and module manifests target the published TinyBus 0.3.2 line, and
the vendored gitlink pins that source. The original Cargo lockfiles still need
refreshing under the authorized Cargo validation slot.
