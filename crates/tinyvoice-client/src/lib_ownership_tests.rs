//! Alias-handover regression tests for owned TinyVoice session projections.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
}

type SharedPeerState = Arc<Mutex<PeerState>>;

struct OwnershipVoice {
    peer: Peer,
    state: SharedPeerState,
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
            },
        )
        .await?;
    service_b
        .serve_at(
            object_path,
            OwnershipVoice {
                peer: Peer::B,
                state: state_b.clone(),
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

    let client = make_client(alias_proxy);
    let direct_a = make_client(direct_a_proxy);
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

fn make_client(proxy: Proxy) -> TinyVoiceClient {
    let failures = Arc::new(IgnoreFailures);
    let module = ModuleClient::new(FixtureAccess {
        reporter: FailureReporter::new(failures),
    })
    .with_fixture(proxy);
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
            tinyvoice_bus::names::methods::VAD_PUSH,
            tinyvoice_bus::names::methods::VAD_IS_SPEAKING,
            tinyvoice_bus::names::methods::VAD_RESET,
            tinyvoice_bus::names::methods::VAD_CLOSE,
            tinyvoice_bus::names::methods::ROUTE,
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
