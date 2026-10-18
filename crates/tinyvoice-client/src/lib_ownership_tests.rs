//! Alias-handover regression tests for owned TinyVoice session projections.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinybus::broker::Broker;
use tinybus::module::{
    AccessFuture, FailureReport, FailureReporter, FailureSink, ModuleAccess, ModuleClient,
    ProxyFailure, ReleaseRecord,
};
use tinybus::service::Interface;
use tinybus::transport::memory::MemoryBus;
use tinybus::{Connection, InterfaceName, MemberName, ObjectPath, Proxy};
use tinyvoice_bus::{SessionAction, SessionAdvance, SessionInput, SessionPhase, SessionStatus};

const MODULE_ID: &str = "tinyvoice";
const VAD_ID: u64 = 41;
const RECORDER_ID: u64 = 73;
const CAPTURE_ID: &str = "capture-shared";
const HOTKEY_ID: &str = "hotkey-shared";

static RELEASE: ReleaseRecord = ReleaseRecord {
    id: MODULE_ID,
    bus_name: tinyvoice_bus::BUS_NAME,
    object_path: tinyvoice_bus::OBJECT_PATH,
    version: "0.3.2",
    release_url: "https://example.invalid/tinyvoice",
    assets: &[],
};

#[derive(Default)]
struct IgnoreFailures;

impl FailureSink for IgnoreFailures {
    fn capture(&self, _report: &FailureReport<'_>) {}
}

struct FixtureAccess {
    reporter: FailureReporter,
}

impl ModuleAccess for FixtureAccess {
    fn find(&self, id: &str) -> Option<&'static ReleaseRecord> {
        (id == MODULE_ID).then_some(&RELEASE)
    }

    fn enabled(&self) -> bool {
        true
    }

    fn reporter(&self) -> &FailureReporter {
        &self.reporter
    }

    fn ensure_loaded<'a>(
        &'a self,
        _id: &'a str,
    ) -> AccessFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn proxy<'a>(
        &'a self,
        _bus_name: &'a str,
        _object_path: &'a str,
    ) -> AccessFuture<'a, std::result::Result<Proxy, ProxyFailure>> {
        Box::pin(async { Err(ProxyFailure::Transport) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Peer {
    A,
    B,
}

#[derive(Default)]
struct PeerState {
    calls: Vec<(String, Value)>,
    vad_open: bool,
    recorders: HashMap<u64, SessionStatus>,
    capture_reserved: bool,
    capture_open: bool,
    hotkey_open: bool,
    hotkey_active: bool,
}

type SharedPeerState = Arc<Mutex<PeerState>>;

#[derive(Default)]
struct CaptureStartGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    completed: tokio::sync::Notify,
}

struct OwnershipVoice {
    peer: Peer,
    state: SharedPeerState,
    capture_start_gate: Option<Arc<CaptureStartGate>>,
}

#[async_trait]
impl Interface for OwnershipVoice {
    fn name(&self) -> InterfaceName {
        InterfaceName::new(tinyvoice_bus::BUS_NAME).unwrap_or_else(|_| std::process::abort())
    }

    fn members(&self) -> Vec<MemberName> {
        tinyvoice_bus::METHODS
            .iter()
            .filter_map(|name| MemberName::new(*name).ok())
            .collect()
    }

    async fn call(&self, member: &MemberName, args: Value) -> tinybus::Result<Value> {
        let method = member.as_str();
        if method == tinyvoice_bus::names::methods::CAPTURE_START {
            if let Some(gate) = &self.capture_start_gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| tinybus::Error::failed("ownership fixture state is poisoned"))?;
        state.calls.push((method.to_owned(), args.clone()));
        let values = args
            .as_array()
            .ok_or_else(|| tinybus::Error::failed("expected positional method arguments"))?;

        let result = match method {
            tinyvoice_bus::names::methods::ROUTE => match self.peer {
                Peer::A => json!(r#"{"intent":"pause"}"#),
                Peer::B => json!(r#"{"intent":"resume"}"#),
            },
            tinyvoice_bus::names::methods::VAD_OPEN => {
                state.vad_open = true;
                json!(VAD_ID)
            }
            tinyvoice_bus::names::methods::VAD_PUSH => {
                let id = positional_id(values)?;
                ensure_vad_open(&state, id)?;
                let events = match self.peer {
                    Peer::A => r#"[{"frame":0,"kind":"speech_start"}]"#,
                    Peer::B => {
                        r#"[{"frame":0,"kind":"speech_end","voiced_ms":20,"emit":true,"forced":false}]"#
                    }
                };
                json!(events)
            }
            tinyvoice_bus::names::methods::VAD_IS_SPEAKING => {
                let id = positional_id(values)?;
                ensure_vad_open(&state, id)?;
                json!(self.peer == Peer::B)
            }
            tinyvoice_bus::names::methods::VAD_RESET => {
                let id = positional_id(values)?;
                ensure_vad_open(&state, id)?;
                Value::Null
            }
            tinyvoice_bus::names::methods::VAD_CLOSE => {
                let id = positional_id(values)?;
                ensure_vad_open(&state, id)?;
                state.vad_open = false;
                Value::Null
            }
            tinyvoice_bus::names::methods::SESSION_OPEN => {
                state.recorders.insert(
                    RECORDER_ID,
                    SessionStatus {
                        phase: SessionPhase::Stopped,
                        generation: 0,
                    },
                );
                json!(RECORDER_ID)
            }
            tinyvoice_bus::names::methods::SESSION_ADVANCE => {
                let id = positional_id(values)?;
                let input_json = values
                    .get(1)
                    .and_then(Value::as_str)
                    .ok_or_else(|| tinybus::Error::failed("missing recorder input"))?;
                let input: SessionInput = serde_json::from_str(input_json)
                    .map_err(|_| tinybus::Error::failed("invalid recorder input"))?;
                let status = state
                    .recorders
                    .get_mut(&id)
                    .ok_or_else(|| tinybus::Error::failed("unknown recorder id"))?;
                let reply = advance_fixture_session(status, input);
                let encoded = serde_json::to_string(&reply)
                    .map_err(|_| tinybus::Error::failed("could not encode recorder reply"))?;
                json!(encoded)
            }
            tinyvoice_bus::names::methods::SESSION_STATUS => {
                let id = positional_id(values)?;
                let status = state
                    .recorders
                    .get(&id)
                    .ok_or_else(|| tinybus::Error::failed("unknown recorder id"))?;
                let encoded = serde_json::to_string(status)
                    .map_err(|_| tinybus::Error::failed("could not encode recorder status"))?;
                json!(encoded)
            }
            tinyvoice_bus::names::methods::SESSION_CLOSE => {
                let id = positional_id(values)?;
                state
                    .recorders
                    .remove(&id)
                    .ok_or_else(|| tinybus::Error::failed("unknown recorder id"))?;
                Value::Null
            }
            tinyvoice_bus::names::methods::RESERVE_CAPTURE => {
                state.capture_reserved = true;
                json!({"Ok": CAPTURE_ID})
            }
            tinyvoice_bus::names::methods::CAPTURE_START => {
                let request = values
                    .first()
                    .and_then(Value::as_object)
                    .ok_or_else(|| tinybus::Error::failed("missing capture start request"))?;
                let handle = request
                    .get("handle")
                    .and_then(Value::as_str)
                    .ok_or_else(|| tinybus::Error::failed("missing capture reservation"))?;
                if !state.capture_reserved || handle != CAPTURE_ID {
                    return Err(tinybus::Error::failed("unknown capture reservation"));
                }
                state.capture_open = true;
                if let Some(gate) = &self.capture_start_gate {
                    gate.completed.notify_one();
                }
                json!({
                    "Ok": {
                        "handle": CAPTURE_ID,
                        "format": {"source_rate": 48_000, "channels": 1}
                    }
                })
            }
            tinyvoice_bus::names::methods::CAPTURE_POLL => {
                let request = values
                    .first()
                    .and_then(Value::as_object)
                    .ok_or_else(|| tinybus::Error::failed("missing capture poll request"))?;
                ensure_capture_open(&state, request.get("handle").and_then(Value::as_str))?;
                let sample = if self.peer == Peer::A { 0.25 } else { 0.75 };
                json!({
                    "Ok": {
                        "chunks": [{"samples": [sample]}],
                        "closed": false
                    }
                })
            }
            tinyvoice_bus::names::methods::CAPTURE_STOP => {
                ensure_capture_open(&state, values.first().and_then(Value::as_str))?;
                state.capture_open = false;
                json!({"Ok": null})
            }
            tinyvoice_bus::names::methods::HOTKEY_RESERVE => {
                state.hotkey_open = true;
                json!({"Ok": HOTKEY_ID})
            }
            tinyvoice_bus::names::methods::HOTKEY_START => {
                ensure_hotkey_open(&state, values.first().and_then(Value::as_object))?;
                json!({
                    "Ok": {
                        "state": "running",
                        "generation": 1,
                        "active": false,
                        "continuity_lost": false
                    }
                })
            }
            tinyvoice_bus::names::methods::HOTKEY_FEED => {
                ensure_hotkey_open(&state, values.first().and_then(Value::as_object))?;
                state.hotkey_active = true;
                json!({
                    "Ok": {
                        "state": "running",
                        "generation": 1,
                        "active": true,
                        "continuity_lost": false
                    }
                })
            }
            tinyvoice_bus::names::methods::HOTKEY_READ => {
                ensure_hotkey_open(&state, values.first().and_then(Value::as_object))?;
                let event = if self.peer == Peer::A {
                    "pressed"
                } else {
                    "released"
                };
                let active = self.peer == Peer::B && state.hotkey_active;
                json!({
                    "Ok": {
                        "generation": 1,
                        "batch": 1,
                        "events": [{"generation": 1, "sequence": 1, "event": event}],
                        "reset": false,
                        "active": active
                    }
                })
            }
            tinyvoice_bus::names::methods::HOTKEY_STOP => {
                ensure_hotkey_open(&state, values.first().and_then(Value::as_object))?;
                state.hotkey_open = false;
                json!({
                    "Ok": {
                        "state": "stopped",
                        "generation": 1,
                        "active": false,
                        "continuity_lost": false
                    }
                })
            }
            _ => {
                return Err(tinybus::Error::failed(
                    "unexpected ownership fixture method",
                ));
            }
        };

        Ok(result)
    }
}

fn positional_id(values: &[Value]) -> tinybus::Result<u64> {
    values
        .first()
        .and_then(Value::as_u64)
        .ok_or_else(|| tinybus::Error::failed("missing resource id"))
}

fn ensure_vad_open(state: &PeerState, id: u64) -> tinybus::Result<()> {
    if id == VAD_ID && state.vad_open {
        Ok(())
    } else {
        Err(tinybus::Error::failed("unknown VAD id"))
    }
}

fn ensure_capture_open(state: &PeerState, handle: Option<&str>) -> tinybus::Result<()> {
    if handle == Some(CAPTURE_ID) && state.capture_open {
        Ok(())
    } else {
        Err(tinybus::Error::failed("unknown capture stream"))
    }
}

fn ensure_hotkey_open(
    state: &PeerState,
    request: Option<&serde_json::Map<String, Value>>,
) -> tinybus::Result<()> {
    if request
        .and_then(|request| request.get("handle"))
        .and_then(Value::as_str)
        == Some(HOTKEY_ID)
        && state.hotkey_open
    {
        Ok(())
    } else {
        Err(tinybus::Error::failed("unknown hotkey handle"))
    }
}

fn advance_fixture_session(status: &mut SessionStatus, input: SessionInput) -> SessionAdvance {
    let (accepted, action) = match input {
        SessionInput::Run if status.phase == SessionPhase::Stopped => {
            status.phase = SessionPhase::Idle;
            (true, SessionAction::None)
        }
        SessionInput::Pressed if status.phase == SessionPhase::Idle => {
            status.phase = SessionPhase::Recording;
            status.generation += 1;
            (
                true,
                SessionAction::StartRecording {
                    generation: status.generation,
                },
            )
        }
        SessionInput::SetupReady {
            stop_buffered: false,
        } if status.phase == SessionPhase::Recording => (
            true,
            SessionAction::RecordingLive {
                generation: status.generation,
            },
        ),
        _ => (false, SessionAction::None),
    };

    SessionAdvance {
        accepted,
        phase: status.phase,
        generation: status.generation,
        action,
    }
}

struct AliasHandoffFixture {
    client: TinyVoiceClient,
    direct_a: TinyVoiceClient,
    caller: Connection,
    service_a: Connection,
    service_b: Connection,
    state_a: SharedPeerState,
    state_b: SharedPeerState,
}

async fn handoff_fixture() -> tinybus::Result<AliasHandoffFixture> {
    handoff_fixture_with_capture_start_gate(None, None).await
}

async fn handoff_fixture_with_capture_start_gate(
    capture_start_gate: Option<Arc<CaptureStartGate>>,
    client_timeout: Option<Duration>,
) -> tinybus::Result<AliasHandoffFixture> {
    let bus = MemoryBus::new();
    Broker::new().spawn(bus.clone());

    let service_a = Connection::connect(bus.connect().await?).await?;
    let service_b = Connection::connect(bus.connect().await?).await?;
    let state_a = Arc::new(Mutex::new(PeerState::default()));
    let state_b = Arc::new(Mutex::new(PeerState::default()));
    let object_path = ObjectPath::new(tinyvoice_bus::OBJECT_PATH)?;

    service_a
        .serve_at(
            object_path.clone(),
            OwnershipVoice {
                peer: Peer::A,
                state: state_a.clone(),
                capture_start_gate: capture_start_gate.clone(),
            },
        )
        .await?;
    service_b
        .serve_at(
            object_path,
            OwnershipVoice {
                peer: Peer::B,
                state: state_b.clone(),
                capture_start_gate: None,
            },
        )
        .await?;
    service_a.request_name(tinyvoice_bus::BUS_NAME).await?;

    let caller = Connection::connect(bus.connect().await?).await?;
    let alias_proxy = caller.proxy(
        tinyvoice_bus::BUS_NAME,
        tinyvoice_bus::OBJECT_PATH,
        tinyvoice_bus::BUS_NAME,
    )?;
    let service_a_unique = service_a
        .unique_name()
        .ok_or_else(|| tinybus::Error::failed("service A has no unique name"))?;
    let direct_a_proxy = caller.proxy(
        service_a_unique.as_str(),
        tinyvoice_bus::OBJECT_PATH,
        tinyvoice_bus::BUS_NAME,
    )?;

    let client = make_client_with_timeout(alias_proxy, client_timeout);
    let direct_a = make_client_with_timeout(direct_a_proxy, client_timeout);
    Ok(AliasHandoffFixture {
        client,
        direct_a,
        caller,
        service_a,
        service_b,
        state_a,
        state_b,
    })
}

fn make_client_with_timeout(proxy: Proxy, timeout: Option<Duration>) -> TinyVoiceClient {
    let failures = Arc::new(IgnoreFailures);
    let mut module = ModuleClient::new(FixtureAccess {
        reporter: FailureReporter::new(failures),
    })
    .with_fixture(proxy);
    if let Some(timeout) = timeout {
        module = module.with_timeout(timeout);
    }
    TinyVoiceClient::new(module)
}

impl AliasHandoffFixture {
    async fn move_public_alias_to_b(&self) -> tinybus::Result<()> {
        self.service_a.release_name(tinyvoice_bus::BUS_NAME).await?;
        self.service_b.request_name(tinyvoice_bus::BUS_NAME).await?;
        let owner = self.caller.name_owner(tinyvoice_bus::BUS_NAME).await?;
        if owner != self.service_b.unique_name() {
            return Err(tinybus::Error::failed(
                "public TinyVoice alias did not move to service B",
            ));
        }
        Ok(())
    }
}

fn methods(state: &SharedPeerState) -> Vec<String> {
    state
        .lock()
        .expect("ownership fixture state lock")
        .calls
        .iter()
        .map(|(method, _)| method.clone())
        .collect()
}

#[tokio::test]
async fn vad_session_stays_with_its_creator_across_a_live_public_alias_handover() {
    let fixture = handoff_fixture()
        .await
        .expect("start both service peers on one broker");
    let a = fixture
        .client
        .open_vad(VadConfig::default())
        .await
        .expect("open VAD session on A");
    assert_eq!(a.id, VAD_ID);

    fixture
        .move_public_alias_to_b()
        .await
        .expect("move public name while A remains connected");
    assert_eq!(
        fixture
            .direct_a
            .route("prove service A is still live")
            .await
            .expect("A stays directly callable by unique name"),
        VoiceIntent::Pause
    );
    assert_eq!(
        fixture
            .client
            .route("prove replacement service B is usable")
            .await
            .expect("the public alias now reaches B"),
        VoiceIntent::Resume
    );

    let b = fixture
        .client
        .open_vad(VadConfig::default())
        .await
        .expect("open colliding VAD session on B");
    assert_eq!(b.id, a.id, "module-local VAD ids collide across peers");
    let b_push = b.push(20, &[0.8]).await;
    let b_speaking = b.is_speaking().await;
    assert_eq!(
        b_push,
        Ok(vec![IndexedVadEvent {
            frame: 0,
            event: tinyvoice_bus::VadEvent::SpeechEnd {
                voiced_ms: 20,
                emit: true,
                forced: false,
            },
        }]),
        "B returns a usable B-specific VAD canary"
    );
    assert_eq!(b_speaking, Ok(true));

    // Run every A operation before asserting so the ingress evidence captures
    // push, read, reset, and close even while this regression is still present.
    let a_push = a.push(20, &[0.8]).await;
    let a_speaking = a.is_speaking().await;
    let a_reset = a.reset().await;
    let a_close = a.close().await;
    let b_still_open = b.is_speaking().await;

    assert_eq!(
        (a_push, a_speaking, a_reset, a_close, b_still_open),
        (
            Ok(vec![IndexedVadEvent {
                frame: 0,
                event: tinyvoice_bus::VadEvent::SpeechStart,
            }]),
            Ok(false),
            Ok(()),
            Ok(()),
            Ok(true),
        ),
        "A's owner projection must remain on A; closing A must leave B live"
    );
    assert_eq!(
        methods(&fixture.state_a),
        [
            tinyvoice_bus::names::methods::VAD_OPEN,
            tinyvoice_bus::names::methods::ROUTE,
            tinyvoice_bus::names::methods::VAD_PUSH,
            tinyvoice_bus::names::methods::VAD_IS_SPEAKING,
            tinyvoice_bus::names::methods::VAD_RESET,
            tinyvoice_bus::names::methods::VAD_CLOSE,
        ]
        .map(str::to_owned)
    );
    assert_eq!(
        methods(&fixture.state_b),
        [
            tinyvoice_bus::names::methods::ROUTE,
            tinyvoice_bus::names::methods::VAD_OPEN,
            tinyvoice_bus::names::methods::VAD_PUSH,
            tinyvoice_bus::names::methods::VAD_IS_SPEAKING,
            tinyvoice_bus::names::methods::VAD_IS_SPEAKING,
        ]
        .map(str::to_owned),
        "B receives its own usable calls and no call from A's projection"
    );
}

#[tokio::test]
async fn recorder_session_stays_with_its_creator_across_a_live_public_alias_handover() {
    let fixture = handoff_fixture()
        .await
        .expect("start both service peers on one broker");
    let a = RecorderSession::open(&fixture.client)
        .await
        .expect("open recorder on A");
    assert_eq!(a.id, RECORDER_ID);

    fixture
        .move_public_alias_to_b()
        .await
        .expect("move public name while A remains connected");
    assert_eq!(
        fixture
            .direct_a
            .route("prove service A is still live")
            .await
            .expect("A stays directly callable by unique name"),
        VoiceIntent::Pause
    );
    assert_eq!(
        fixture
            .client
            .route("prove replacement service B is usable")
            .await
            .expect("the public alias now reaches B"),
        VoiceIntent::Resume
    );

    let b = RecorderSession::open(&fixture.client)
        .await
        .expect("open colliding recorder on B");
    assert_eq!(b.id, a.id, "module-local recorder ids collide across peers");
    let b_run = b.advance(SessionInput::Run).await;
    let b_pressed = b.advance(SessionInput::Pressed).await;
    let b_ready = b
        .advance(SessionInput::SetupReady {
            stop_buffered: false,
        })
        .await;
    let b_status = b.status().await;
    assert_eq!(
        (b_run, b_pressed, b_ready, b_status),
        (
            Ok(SessionAdvance {
                accepted: true,
                phase: SessionPhase::Idle,
                generation: 0,
                action: SessionAction::None,
            }),
            Ok(SessionAdvance {
                accepted: true,
                phase: SessionPhase::Recording,
                generation: 1,
                action: SessionAction::StartRecording { generation: 1 },
            }),
            Ok(SessionAdvance {
                accepted: true,
                phase: SessionPhase::Recording,
                generation: 1,
                action: SessionAction::RecordingLive { generation: 1 },
            }),
            Ok(SessionStatus {
                phase: SessionPhase::Recording,
                generation: 1,
            }),
        ),
        "B's colliding session is live and returns valid B-side behavior"
    );

    // Run all A operations after B owns the public alias, then check the A
    // canaries and exact endpoint ingress together.
    let a_run = a.advance(SessionInput::Run).await;
    let a_status = a.status().await;
    let a_close = a.close().await;
    let b_still_open = b.status().await;
    assert_eq!(
        (a_run, a_status, a_close, b_still_open),
        (
            Ok(SessionAdvance {
                accepted: true,
                phase: SessionPhase::Idle,
                generation: 0,
                action: SessionAction::None,
            }),
            Ok(SessionStatus {
                phase: SessionPhase::Idle,
                generation: 0,
            }),
            Ok(()),
            Ok(SessionStatus {
                phase: SessionPhase::Recording,
                generation: 1,
            }),
        ),
        "A's owner projection must stay on A; closing A must leave B live"
    );
    assert_eq!(
        methods(&fixture.state_a),
        [
            tinyvoice_bus::names::methods::SESSION_OPEN,
            tinyvoice_bus::names::methods::ROUTE,
            tinyvoice_bus::names::methods::SESSION_ADVANCE,
            tinyvoice_bus::names::methods::SESSION_STATUS,
            tinyvoice_bus::names::methods::SESSION_CLOSE,
        ]
        .map(str::to_owned)
    );
    assert_eq!(
        methods(&fixture.state_b),
        [
            tinyvoice_bus::names::methods::ROUTE,
            tinyvoice_bus::names::methods::SESSION_OPEN,
            tinyvoice_bus::names::methods::SESSION_ADVANCE,
            tinyvoice_bus::names::methods::SESSION_ADVANCE,
            tinyvoice_bus::names::methods::SESSION_ADVANCE,
            tinyvoice_bus::names::methods::SESSION_STATUS,
            tinyvoice_bus::names::methods::SESSION_STATUS,
        ]
        .map(str::to_owned),
        "B receives its own usable calls and no call from A's projection"
    );
}

#[tokio::test]
async fn owned_capture_stream_stays_with_its_creator_across_alias_handover() {
    let fixture = handoff_fixture()
        .await
        .expect("start both service peers on one broker");
    let a = fixture
        .client
        .start_capture_owned(MicrophonePermission::Granted)
        .await
        .expect("start capture on A")
        .expect("A capture start succeeds");
    assert_eq!(
        a.format(),
        CaptureFormat {
            source_rate: 48_000,
            channels: 1,
        }
    );

    fixture
        .move_public_alias_to_b()
        .await
        .expect("move public name while A remains connected");
    assert_eq!(
        fixture
            .direct_a
            .route("prove service A is still live")
            .await
            .expect("A stays directly callable by unique name"),
        VoiceIntent::Pause
    );
    assert_eq!(
        fixture
            .client
            .route("prove replacement service B is usable")
            .await
            .expect("the public alias now reaches B"),
        VoiceIntent::Resume
    );

    let b = fixture
        .client
        .start_capture_owned(MicrophonePermission::Granted)
        .await
        .expect("start colliding capture on B")
        .expect("B capture start succeeds");
    assert_eq!(a.handle, b.handle, "capture handles collide across peers");
    let b_batch = b.poll(1).await;
    assert_eq!(
        b_batch,
        Ok(Ok(CaptureBatch {
            chunks: vec![RawChunk {
                samples: vec![0.75]
            }],
            closed: false,
        })),
        "B's stream returns a usable B-specific capture canary"
    );

    let a_batch = a.poll(1).await;
    let a_close = a.close().await;
    let b_still_open = b.poll(1).await;
    assert_eq!(
        (a_batch, a_close, b_still_open),
        (
            Ok(Ok(CaptureBatch {
                chunks: vec![RawChunk {
                    samples: vec![0.25]
                }],
                closed: false,
            })),
            Ok(Ok(())),
            Ok(Ok(CaptureBatch {
                chunks: vec![RawChunk {
                    samples: vec![0.75]
                }],
                closed: false,
            })),
        ),
        "A polling/close stay on A, and closing A leaves B live"
    );
    assert_eq!(
        methods(&fixture.state_a),
        [
            tinyvoice_bus::names::methods::RESERVE_CAPTURE,
            tinyvoice_bus::names::methods::CAPTURE_START,
            tinyvoice_bus::names::methods::ROUTE,
            tinyvoice_bus::names::methods::CAPTURE_POLL,
            tinyvoice_bus::names::methods::CAPTURE_STOP,
        ]
        .map(str::to_owned)
    );
    assert_eq!(
        methods(&fixture.state_b),
        [
            tinyvoice_bus::names::methods::ROUTE,
            tinyvoice_bus::names::methods::RESERVE_CAPTURE,
            tinyvoice_bus::names::methods::CAPTURE_START,
            tinyvoice_bus::names::methods::CAPTURE_POLL,
            tinyvoice_bus::names::methods::CAPTURE_POLL,
        ]
        .map(str::to_owned),
        "B receives its own usable calls and no call from A's projection"
    );
}

#[tokio::test(start_paused = true)]
async fn timed_out_capture_start_does_not_leave_an_unowned_stream() {
    let gate = Arc::new(CaptureStartGate::default());
    let fixture =
        handoff_fixture_with_capture_start_gate(Some(gate.clone()), Some(Duration::from_secs(1)))
            .await
            .expect("start gated capture service");
    let client = fixture.client.clone();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = client
            .start_capture_owned(MicrophonePermission::Granted)
            .await;
        let _ = result_tx.send(result);
    });

    // The reservation call has completed and CAPTURE_START has reached the
    // remote service, which is deliberately held before native publication.
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("CAPTURE_START reaches the delayed service");
    tokio::time::advance(Duration::from_secs(1)).await;
    let timed_out = tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("the caller receives its deadline result")
        .expect("the client task reports its deadline result");
    assert!(
        timed_out.is_err(),
        "the start RPC must hit its configured deadline"
    );

    // A timeout only stops the caller's wait. The in-flight service operation
    // can still publish the stream after the caller lost the private handle.
    gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), gate.completed.notified())
        .await
        .expect("the delayed service finishes publishing the stream");
    let late_poll = fixture
        .client
        .poll_capture(CapturePollRequest {
            handle: CaptureHandle(CAPTURE_ID.to_owned()),
            max_chunks: 1,
        })
        .await;
    let late_stream_is_live = matches!(late_poll, Ok(Ok(_)));
    let cleanup = fixture
        .client
        .stop_capture(CaptureHandle(CAPTURE_ID.to_owned()))
        .await;

    assert!(cleanup.is_ok(), "the fixture cleans up any late stream");
    assert!(
        !late_stream_is_live,
        "a timed-out owned start must not leave an unowned live stream"
    );
    assert_eq!(
        methods(&fixture.state_a),
        [
            tinyvoice_bus::names::methods::RESERVE_CAPTURE,
            tinyvoice_bus::names::methods::CAPTURE_START,
            tinyvoice_bus::names::methods::CAPTURE_POLL,
            tinyvoice_bus::names::methods::CAPTURE_STOP,
        ]
        .map(str::to_owned)
    );
}

#[tokio::test]
async fn canceled_capture_start_does_not_leave_an_unowned_stream() {
    let gate = Arc::new(CaptureStartGate::default());
    let fixture = handoff_fixture_with_capture_start_gate(Some(gate.clone()), None)
        .await
        .expect("start gated capture service");
    let client = fixture.client.clone();
    let start_task = tokio::spawn(async move {
        client
            .start_capture_owned(MicrophonePermission::Granted)
            .await
    });

    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("CAPTURE_START reaches the delayed service");
    start_task.abort();
    let aborted = tokio::time::timeout(Duration::from_secs(5), start_task)
        .await
        .expect("the caller task observes cancellation")
        .expect_err("the caller task is canceled")
        .is_cancelled();

    gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), gate.completed.notified())
        .await
        .expect("the delayed service finishes publishing the stream");
    let late_poll = fixture
        .client
        .poll_capture(CapturePollRequest {
            handle: CaptureHandle(CAPTURE_ID.to_owned()),
            max_chunks: 1,
        })
        .await;
    let late_stream_is_live = matches!(late_poll, Ok(Ok(_)));
    let cleanup = fixture
        .client
        .stop_capture(CaptureHandle(CAPTURE_ID.to_owned()))
        .await;
    assert!(aborted, "the caller's start future was canceled");
    assert!(cleanup.is_ok(), "the fixture cleans up any late stream");
    assert!(
        !late_stream_is_live,
        "a canceled owned start must not leave an unowned live stream"
    );
}

#[tokio::test]
async fn owned_hotkey_session_stays_with_its_creator_across_alias_handover() {
    let fixture = handoff_fixture()
        .await
        .expect("start both service peers on one broker");
    let request = HotkeyRequest {
        key: "Fn".to_owned(),
        mode: ActivationMode::Push,
        source: HotkeySource::Host,
    };
    let a = fixture
        .client
        .reserve_hotkey_owned(request.clone())
        .await
        .expect("reserve hotkey on A")
        .expect("A hotkey reservation succeeds");

    fixture
        .move_public_alias_to_b()
        .await
        .expect("move public name while A remains connected");
    assert_eq!(
        fixture
            .direct_a
            .route("prove service A is still live")
            .await
            .expect("A stays directly callable by unique name"),
        VoiceIntent::Pause
    );
    assert_eq!(
        fixture
            .client
            .route("prove replacement service B is usable")
            .await
            .expect("the public alias now reaches B"),
        VoiceIntent::Resume
    );

    let b = fixture
        .client
        .reserve_hotkey_owned(request)
        .await
        .expect("reserve colliding hotkey on B")
        .expect("B hotkey reservation succeeds");
    assert_eq!(a.handle, b.handle, "hotkey handles collide across peers");
    let b_started = b.start().await;
    let b_feed = b
        .feed(
            1,
            vec![SequencedHostFact {
                sequence: 1,
                fact: HostKeyFact::Down,
            }],
            false,
        )
        .await;
    let b_batch = b.read(None).await;
    assert_eq!(
        (b_started, b_feed, b_batch),
        (
            Ok(Ok(HotkeyReply {
                state: tinyvoice_bus::HotkeyState::Running,
                generation: Some(1),
                active: false,
                continuity_lost: false,
            })),
            Ok(Ok(HotkeyReply {
                state: tinyvoice_bus::HotkeyState::Running,
                generation: Some(1),
                active: true,
                continuity_lost: false,
            })),
            Ok(Ok(HotkeyBatch {
                generation: 1,
                batch: 1,
                events: vec![tinyvoice_bus::hotkey::SequencedHotkeyEvent {
                    generation: 1,
                    sequence: 1,
                    event: HotkeyEvent::Released,
                }],
                reset: false,
                active: true,
            })),
        ),
        "B's start/feed/read path returns usable B-specific canaries"
    );

    let a_started = a.start().await;
    let a_feed = a
        .feed(
            1,
            vec![SequencedHostFact {
                sequence: 1,
                fact: HostKeyFact::Down,
            }],
            false,
        )
        .await;
    let a_batch = a.read(None).await;
    let a_close = a.close().await;
    let b_still_open = b.read(None).await;
    assert_eq!(
        (a_started, a_feed, a_batch, a_close, b_still_open),
        (
            Ok(Ok(HotkeyReply {
                state: tinyvoice_bus::HotkeyState::Running,
                generation: Some(1),
                active: false,
                continuity_lost: false,
            })),
            Ok(Ok(HotkeyReply {
                state: tinyvoice_bus::HotkeyState::Running,
                generation: Some(1),
                active: true,
                continuity_lost: false,
            })),
            Ok(Ok(HotkeyBatch {
                generation: 1,
                batch: 1,
                events: vec![tinyvoice_bus::hotkey::SequencedHotkeyEvent {
                    generation: 1,
                    sequence: 1,
                    event: HotkeyEvent::Pressed,
                }],
                reset: false,
                active: false,
            })),
            Ok(Ok(HotkeyReply {
                state: tinyvoice_bus::HotkeyState::Stopped,
                generation: Some(1),
                active: false,
                continuity_lost: false,
            })),
            Ok(Ok(HotkeyBatch {
                generation: 1,
                batch: 1,
                events: vec![tinyvoice_bus::hotkey::SequencedHotkeyEvent {
                    generation: 1,
                    sequence: 1,
                    event: HotkeyEvent::Released,
                }],
                reset: false,
                active: true,
            })),
        ),
        "A's listener operations stay on A, and closing A leaves B live"
    );
    assert_eq!(
        methods(&fixture.state_a),
        [
            tinyvoice_bus::names::methods::HOTKEY_RESERVE,
            tinyvoice_bus::names::methods::ROUTE,
            tinyvoice_bus::names::methods::HOTKEY_START,
            tinyvoice_bus::names::methods::HOTKEY_FEED,
            tinyvoice_bus::names::methods::HOTKEY_READ,
            tinyvoice_bus::names::methods::HOTKEY_STOP,
        ]
        .map(str::to_owned)
    );
    assert_eq!(
        methods(&fixture.state_b),
        [
            tinyvoice_bus::names::methods::ROUTE,
            tinyvoice_bus::names::methods::HOTKEY_RESERVE,
            tinyvoice_bus::names::methods::HOTKEY_START,
            tinyvoice_bus::names::methods::HOTKEY_FEED,
            tinyvoice_bus::names::methods::HOTKEY_READ,
            tinyvoice_bus::names::methods::HOTKEY_READ,
        ]
        .map(str::to_owned),
        "B receives its own usable calls and no call from A's projection"
    );
}

#[tokio::test]
async fn owned_resources_fail_closed_after_their_peer_detaches() {
    let fixture = handoff_fixture()
        .await
        .expect("start both service peers on one broker");
    let a_vad = fixture
        .client
        .open_vad(VadConfig::default())
        .await
        .expect("open A VAD");
    let a_recorder = RecorderSession::open(&fixture.client)
        .await
        .expect("open A recorder");
    let a_capture = fixture
        .client
        .start_capture_owned(MicrophonePermission::Granted)
        .await
        .expect("start A capture")
        .expect("A capture starts");
    let a_hotkey = fixture
        .client
        .reserve_hotkey_owned(HotkeyRequest {
            key: "Fn".to_owned(),
            mode: ActivationMode::Push,
            source: HotkeySource::Host,
        })
        .await
        .expect("reserve A hotkey")
        .expect("A hotkey reserves");

    fixture
        .move_public_alias_to_b()
        .await
        .expect("move alias while A remains live");
    assert_eq!(
        fixture
            .direct_a
            .route("A unique-peer positive control")
            .await
            .expect("A remains callable before detach"),
        VoiceIntent::Pause
    );
    let b_vad = fixture
        .client
        .open_vad(VadConfig::default())
        .await
        .expect("open colliding B VAD");
    let b_recorder = RecorderSession::open(&fixture.client)
        .await
        .expect("open colliding B recorder");
    let b_capture = fixture
        .client
        .start_capture_owned(MicrophonePermission::Granted)
        .await
        .expect("start colliding B capture")
        .expect("B capture starts");
    let b_hotkey = fixture
        .client
        .reserve_hotkey_owned(HotkeyRequest {
            key: "Fn".to_owned(),
            mode: ActivationMode::Push,
            source: HotkeySource::Host,
        })
        .await
        .expect("reserve colliding B hotkey")
        .expect("B hotkey reserves");
    assert_eq!(a_vad.id, b_vad.id);
    assert_eq!(a_recorder.id, b_recorder.id);
    assert_eq!(a_capture.handle, b_capture.handle);
    assert_eq!(a_hotkey.handle, b_hotkey.handle);

    b_recorder
        .advance(SessionInput::Run)
        .await
        .expect("B recorder responds");
    b_hotkey.start().await.expect("B hotkey starts");
    b_hotkey
        .feed(
            1,
            vec![SequencedHostFact {
                sequence: 1,
                fact: HostKeyFact::Down,
            }],
            false,
        )
        .await
        .expect("B accepts host-owned input");
    assert_eq!(
        b_vad.push(20, &[0.8]).await.expect("B VAD is usable").len(),
        1
    );
    assert!(
        b_capture
            .poll(1)
            .await
            .expect("B capture is usable")
            .is_ok()
    );
    assert!(
        b_hotkey
            .read(None)
            .await
            .expect("B hotkey is usable")
            .is_ok()
    );

    fixture
        .service_a
        .close()
        .await
        .expect("detach A's service connection");
    assert_eq!(
        fixture
            .client
            .route("B alias remains usable after A detaches")
            .await
            .expect("B remains the public alias owner"),
        VoiceIntent::Resume
    );

    let a_vad_push = a_vad.push(20, &[0.8]).await;
    let a_vad_speaking = a_vad.is_speaking().await;
    let a_vad_reset = a_vad.reset().await;
    let a_vad_close = a_vad.close().await;
    let a_recorder_advance = a_recorder.advance(SessionInput::Run).await;
    let a_recorder_status = a_recorder.status().await;
    let a_recorder_close = a_recorder.close().await;
    let a_capture_poll = a_capture.poll(1).await;
    let a_capture_close = a_capture.close().await;
    let a_hotkey_start = a_hotkey.start().await;
    let a_hotkey_feed = a_hotkey
        .feed(
            1,
            vec![SequencedHostFact {
                sequence: 1,
                fact: HostKeyFact::Down,
            }],
            false,
        )
        .await;
    let a_hotkey_read = a_hotkey.read(None).await;
    let a_hotkey_close = a_hotkey.close().await;
    assert!(
        a_vad_push.is_err()
            && a_vad_speaking.is_err()
            && a_vad_reset.is_err()
            && a_vad_close.is_err()
            && a_recorder_advance.is_err()
            && a_recorder_status.is_err()
            && a_recorder_close.is_err()
            && a_capture_poll.is_err()
            && a_capture_close.is_err()
            && a_hotkey_start.is_err()
            && a_hotkey_feed.is_err()
            && a_hotkey_read.is_err()
            && a_hotkey_close.is_err(),
        "all operations against detached A fail instead of falling back to B"
    );

    assert_eq!(
        b_vad.is_speaking().await,
        Ok(true),
        "A's failed VAD operations did not reach or close B"
    );
    assert_eq!(
        b_recorder.status().await,
        Ok(SessionStatus {
            phase: SessionPhase::Idle,
            generation: 0,
        }),
        "A's failed recorder operations did not reach or close B"
    );
    assert!(
        b_capture
            .poll(1)
            .await
            .expect("B capture remains pinned")
            .is_ok()
    );
    assert!(
        b_hotkey
            .read(None)
            .await
            .expect("B hotkey remains pinned")
            .is_ok()
    );
    assert_eq!(
        methods(&fixture.state_b),
        [
            tinyvoice_bus::names::methods::VAD_OPEN,
            tinyvoice_bus::names::methods::SESSION_OPEN,
            tinyvoice_bus::names::methods::RESERVE_CAPTURE,
            tinyvoice_bus::names::methods::CAPTURE_START,
            tinyvoice_bus::names::methods::HOTKEY_RESERVE,
            tinyvoice_bus::names::methods::SESSION_ADVANCE,
            tinyvoice_bus::names::methods::HOTKEY_START,
            tinyvoice_bus::names::methods::HOTKEY_FEED,
            tinyvoice_bus::names::methods::VAD_PUSH,
            tinyvoice_bus::names::methods::CAPTURE_POLL,
            tinyvoice_bus::names::methods::HOTKEY_READ,
            tinyvoice_bus::names::methods::ROUTE,
            tinyvoice_bus::names::methods::VAD_IS_SPEAKING,
            tinyvoice_bus::names::methods::SESSION_STATUS,
            tinyvoice_bus::names::methods::CAPTURE_POLL,
            tinyvoice_bus::names::methods::HOTKEY_READ,
        ]
        .map(str::to_owned),
        "B receives its own lifecycle calls and no A traffic after detach"
    );
}
