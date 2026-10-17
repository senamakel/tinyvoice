# TinyVoice

Host-agnostic voice primitives, extracted from [OpenHuman](https://github.com/tinyhumansai/openhuman): WAV framing, silence gating, voice-activity segmentation, wake-word gating, fast-path command routing, STT hallucination detection, and relay-turn preparation.

Ships two things from one repository:

| Crate | Output | For |
| --- | --- | --- |
| `tinyvoice` (`crates/`) | `rlib` | A host that links the logic in-process |
| `tinyvoice-bus` (`crates/`) | `rlib` | A host that only needs TinyBus names and value types |
| `tinyvoice-client` (`crates/`) | `rlib` | A typed TinyBus client for the loadable TinyVoice module |
| `tinyvoice-module` (`crates/`) | `cdylib` | A host that loads it over the TinyBus module ABI |

## What belongs here, and what does not

The split follows one rule, the same one `tinydocs` and `tinywallet` follow:
**a crate owns what is identical for every host; the host owns what depends on
its own runtime, config, or threat model.**

So the default crate build is synchronous, I/O-free and runtime-free. It does
not open a microphone, call an STT or TTS endpoint, or know what a `Config` is.
The optional `hotkey` feature adds a host-facing global keyboard listener. The
optional `capture` feature adds microphone capture when a host supplies a
permission check.

| Here | With the host |
| --- | --- |
| WAV framing, RMS, resampling, downmix, silence gate; module-owned capture threads | Microphone permission decisions through the computer module |
| VAD segmentation | The capture loop that drives it |
| Wake-word gate, intent routing | What to *do* with an intent |
| Hallucination detection | The STT transport, credentials, and retry policy |
| Relay user-text extraction, readback classification and turn preparation | Local agent execution, product prompts, acknowledgment timing and delivery |
| Portable live controls and module-owned provider/socket coordinator | Prepared credentials, provider selection, current tool policy and transcript persistence |

A visible consequence: `VadConfig` has no constructor that reads a config file.
A host builds one from whatever it persists. A crate that guessed at that shape
would be wrong for every host that guessed differently.

## Use it as a library

```rust
use tinyvoice::{
    intent::{extract_command, route, VoiceIntent},
    transcript::{is_hallucinated, Mode},
};

// An always-on microphone hears the whole room, so the wake word is what
// separates an instruction from a passing conversation.
let Some(command) = extract_command("hey tiny, pause the music", "Hey Tiny") else {
    return; // not addressed to the agent
};

// A model fed near-silence returns stock phrases rather than nothing.
if is_hallucinated(&command, Mode::Conversation) {
    return;
}

assert_eq!(route(&command), VoiceIntent::Pause);
```

Run it: `cargo run -p tinyvoice --example basic`.

`tinyvoice::relay::prepare_turn` extracts user text from serialized messages,
classifies a host-supplied readback prefix, and prepares a provider-neutral
turn. It performs no inference or tool execution. A module host uses the
`PrepareRelayTurn` operation with the minimal `tinyvoice-bus` value types;
OpenHuman supplies its local agent, acknowledgment policy and delivery.
See the [relay contract](docs/specs/relay-turn-preparation.md).

### Optional features

- `hotkey` (off by default): `tinyvoice::hotkey`, a system-wide push-to-talk /
  toggle key listener built on `rdev` (`parse_hotkey`, `start_listener`,
  `ActivationMode`, `HotkeyEvent`). Off by default so the library, and the
  loadable module built from it, carry no OS input hook.
- `capture` (off by default): `tinyvoice::capture`, microphone input on `cpal`:
  `start_recording` for a one-shot recording and `start_capture_stream` for a
  continuous chunk stream with a stoppable thread handle, both returning the device's raw samples for
  `tinyvoice::audio` to process. Microphone permission is a `PermissionCheck`
  the host passes in. Off by default because it links the platform audio stack.

## Use it as a TinyBus module

The module claims `ai.tinyhumans.tinyvoice.Voice` and serves
`/ai/tinyhumans/tinyvoice/Voice`. See [`MODULE.md`](MODULE.md) for installation
and the method list.

**A call costs about 13 µs.** Measured in-process against the real loaded
module (`examples/bench_call.rs`): 13.3 µs per round trip, or 0.066% of a 20 ms
audio frame. A TinyBus module shares the host's address space — a call is a
channel send and a JSON hop, not IPC.

That means a live capture loop can drive the VAD through the bus, and the
session methods (`VadOpen` / `VadPush` / `VadReset` / `VadClose`) exist for it.
An earlier version of this README claimed otherwise, on an assumption rather
than a measurement.

The compiled module owns native capture threads and callbacks. `cpal` delivers
on a realtime thread where blocking causes dropouts, so callbacks forward raw
interleaved samples into a bounded queue. Hosts poll batches and call
`PrepareFrames` from a worker.

### Relay turn coordination

The compiled module also coordinates admitted local-brain turns through
explicit RelayHost callbacks: scheduling, selected progress, an acknowledgment
clock that starts before queueing, and deferred completion. Hosts supply the
native brain and retain correlation, phrase and delivery policy. See the
[relay runtime contract](docs/specs/relay-module-runtime.md).

### Live sessions

The compiled module owns the live provider loop through TinyAgentsLive and a
bounded client/provider coordinator. Hosts reserve a lease, bind mechanical
socket callbacks with `LiveBindTransport`, and supply prepared connection
material and explicit declarations through `LiveStart`. The module handles
polling, ordered effects, replay and cleanup; callbacks retain product policy
and persistence. A declaration never grants execution permission.

The minimal contract also supports manual `LiveSend`/`LiveRead` for hosts that
need a custom transport, with replay until acknowledgment. Those operations
are excluded once a module transport is bound. See the
[live runtime contract](docs/specs/live-module-runtime.md) for bounds and
callback cancellation requirements.

## Layout

```text
crates/tinyvoice/
├── src/                # pure voice primitives and public re-export surface
├── tests/              # integration tests against the library API
└── examples/           # runnable library usage
crates/tinyvoice-bus/
└── src/                # transport-free bus names and serialized vocabulary
crates/tinyvoice-module/
├── src/service/        # bus interface, setup, ABI v1 exports
└── examples/           # local and tagged-release module verification
vendor/tinybus/         # pinned TinyBus submodule (build-time only)
```

After cloning: `git submodule update --init vendor/tinybus`.

## Development

```sh
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --manifest-path crates/tinyvoice-module/Cargo.toml --all-features
```

CI additionally requires 90% line coverage in every source file and a clean
`rustdoc -D warnings`. See [`AGENTS.md`](AGENTS.md) for the conventions.

## License

GPL-3.0-only. See [`LICENSE`](LICENSE).

## Module-owned recording

Contract 1.3 adds device enumeration, reserved startup/cancellation, one-shot recording and continuous capture through opaque
handles. The host supplies a microphone permission grant obtained through
TinyComputer; the compiled module owns the device, audio preparation, bounded
WAV reads, bounded raw chunk polling, cancellation and release. See [capture contract](docs/specs/module-capture.md).
Module-owned hotkeys are specified in [hotkey lifecycle](docs/specs/hotkey-module-lifecycle.md); the optional
`tinyvoice::hotkey` listener remains available to library hosts.
