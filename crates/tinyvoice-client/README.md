# tinyvoice-client

`tinyvoice-client` is the typed host adapter for the loadable TinyVoice module.
It depends on `tinyvoice-bus` for the wire contract and TinyBus SDK for module
calls. It does not depend on the host-agnostic `tinyvoice` implementation or
the loadable `tinyvoice-module` crate.

Construct it with `TinyVoiceClient::new(module_client)` using the host's one
configured `ModuleClient`. That keeps module loading, compatibility checks,
deadlines, and redacted failure reporting on the same path the host already
configures; the client must not open a second bus connection or create a
private resolver. Its methods preserve each contract member's positional
tuple and typed reply.

`VadSession` and `RecorderSession` are explicit async leases. Closing them calls
the corresponding module method; dropping a Rust handle does not imply remote
cleanup. OpenHuman keeps host policy such as volume clamping and conversion
from persisted server configuration into `VadConfig` in its own adapter.

The recording-broker tests use only TinyBus's in-memory transport. They record
actual service invocations and verify the wire arguments and returned types;
they do not load a published artifact, access a microphone, or contact a
backend.

The manifest's TinyBus version range targets the published 0.2.2 client API.
Pin the final constraint to the TinyBus 0.3.x runtime when the runtime SDK and
vendored gitlink are aligned.
