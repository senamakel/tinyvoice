//! Product-facing wire tests over a real in-memory TinyBus broker.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tinybus::broker::Broker;
use tinybus::module::{
    AccessFuture, FailureReport, FailureReporter, FailureSink, ModuleAccess, ModuleCallError,
    ModuleClient, ProxyFailure, ReleaseRecord,
};
use tinybus::service::Interface;
use tinybus::transport::memory::MemoryBus;
use tinybus::{Connection, InterfaceName, MemberName, ObjectPath, Proxy};

use super::*;

const MODULE_ID: &str = "tinyvoice";
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

static RELEASE: ReleaseRecord = ReleaseRecord {
    id: MODULE_ID,
    bus_name: tinyvoice_bus::BUS_NAME,
    object_path: tinyvoice_bus::OBJECT_PATH,
    version: "0.3.2",
    release_url: "https://example.invalid/tinyvoice",
    assets: &[],
};

#[derive(Default)]
struct RecordingFailures(Mutex<Vec<(String, String)>>);

impl FailureSink for RecordingFailures {
    fn capture(&self, report: &FailureReport<'_>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((report.module.to_owned(), report.reason.code().to_owned()));
    }
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

#[derive(Clone, Copy)]
enum FailureMode {
    Unavailable,
    Execution,
}

#[derive(Default)]
struct MockState {
    calls: Vec<(String, Value)>,
    failure: Option<FailureMode>,
    replies: Vec<(String, Value)>,
}

type State = Arc<Mutex<MockState>>;

struct MockVoice {
    state: State,
}

#[async_trait]
impl Interface for MockVoice {
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
        let name = member.as_str();
        let mut state = self
            .state
            .lock()
            .map_err(|_| tinybus::Error::failed("fixture state is poisoned"))?;
        state.calls.push((name.to_owned(), args));

        if let Some(mode) = state.failure {
            let secret = "sensitive transcript marker";
            return Err(match mode {
                FailureMode::Unavailable => tinybus::Error::ModuleUnavailable {
                    module: MODULE_ID.to_owned(),
                    state: "stopped".to_owned(),
                    detail: secret.to_owned(),
                },
                FailureMode::Execution => tinybus::Error::failed(secret),
            });
        }

        if let Some((_, reply)) = state.replies.iter().find(|(method, _)| method == name) {
            return Ok(reply.clone());
        }

        let reply = match name {
            tinyvoice_bus::names::methods::ROUTE => json!(r#"{"intent":"pause"}"#),
            tinyvoice_bus::names::methods::SEQUENCE_UTTERANCE => {
                json!(r#"{"disposition":"command","command":"open slack"}"#)
            }
            tinyvoice_bus::names::methods::GATE_RECORDING => json!("\"pass\""),
            tinyvoice_bus::names::methods::IS_HALLUCINATED => json!(true),
            tinyvoice_bus::names::methods::PREPARE_CAPTURE
            | tinyvoice_bus::names::methods::ENCODE_WAV
            | tinyvoice_bus::names::methods::ENCODE_WAV_PCM16 => json!("AQ=="),
            tinyvoice_bus::names::methods::PREPARE_FRAMES => json!("AAAAAA=="),
            tinyvoice_bus::names::methods::FRAME_ENERGIES => json!([0.25]),
            tinyvoice_bus::names::methods::VAD_OPEN => json!(41),
            tinyvoice_bus::names::methods::VAD_PUSH => json!("[]"),
            tinyvoice_bus::names::methods::VAD_IS_SPEAKING => json!(true),
            tinyvoice_bus::names::methods::SESSION_OPEN => json!(73),
            tinyvoice_bus::names::methods::SESSION_ADVANCE => json!(
                r#"{"accepted":true,"phase":"idle","generation":0,"action":"none"}"#
            ),
            tinyvoice_bus::names::methods::SESSION_STATUS => {
                json!(r#"{"phase":"idle","generation":0}"#)
            }
            tinyvoice_bus::names::methods::RESERVE_CAPTURE => json!({
                "Ok": {"handle": "capture-1"}
            }),
            tinyvoice_bus::names::methods::CAPTURE_START => json!({
                "Ok": {
                    "handle": "capture-1",
                    "format": {"source_rate": 48_000, "channels": 1}
                }
            }),
            tinyvoice_bus::names::methods::CAPTURE_POLL => {
                json!({"Ok": {"chunks": [], "closed": false}})
            }
            tinyvoice_bus::names::methods::VAD_RESET
            | tinyvoice_bus::names::methods::VAD_CLOSE
            | tinyvoice_bus::names::methods::SESSION_CLOSE => Value::Null,
            tinyvoice_bus::names::methods::CAPTURE_STOP
            | tinyvoice_bus::names::methods::CAPTURE_SHUTDOWN => json!({"Ok": null}),
            tinyvoice_bus::names::methods::HOTKEY_RESERVE => json!({"Ok": "hotkey-1"}),
            tinyvoice_bus::names::methods::HOTKEY_START
            | tinyvoice_bus::names::methods::HOTKEY_STOP
            | tinyvoice_bus::names::methods::HOTKEY_SHUTDOWN => json!({
                "Ok": {
                    "state": "running",
                    "generation": 1,
                    "active": false,
                    "continuity_lost": false
                }
            }),
            tinyvoice_bus::names::methods::HOTKEY_READ => json!({
                "Ok": {
                    "generation": 1,
                    "batch": 1,
                    "events": [],
                    "reset": false,
                    "active": false
                }
            }),
            tinyvoice_bus::names::methods::HOTKEY_FEED => json!({
                "Ok": {
                    "state": "running",
                    "generation": 1,
                    "active": false,
                    "continuity_lost": false
                }
            }),
            _ => Value::Null,
        };
        Ok(reply)
    }
}

async fn fixture() -> tinybus::Result<(Connection, TinyVoiceClient, State, Arc<RecordingFailures>)> {
    let bus = MemoryBus::new();
    Broker::new().spawn(bus.clone());
    let service = Connection::connect(bus.connect().await?).await?;
    let state = Arc::new(Mutex::new(MockState::default()));
    service
        .serve_at(
            ObjectPath::new(tinyvoice_bus::OBJECT_PATH)?,
            MockVoice {
                state: Arc::clone(&state),
            },
        )
        .await?;
    service.request_name(tinyvoice_bus::BUS_NAME).await?;
    let caller = Connection::connect(bus.connect().await?).await?;
    let proxy = caller.proxy(
        tinyvoice_bus::BUS_NAME,
        tinyvoice_bus::OBJECT_PATH,
        tinyvoice_bus::BUS_NAME,
    )?;
    let failures = Arc::new(RecordingFailures::default());
    let module = ModuleClient::new(FixtureAccess {
        reporter: FailureReporter::new(failures.clone()),
    })
    .with_fixture(proxy);
    Ok((service, TinyVoiceClient::new(module), state, failures))
}

fn encoded(samples: &[f32]) -> String {
    use base64::Engine as _;
    let bytes: Vec<u8> = samples.iter().flat_map(|sample| sample.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn recorded(state: &State) -> Vec<(String, Value)> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .calls
        .clone()
}

#[tokio::test]
async fn stateless_wrappers_preserve_every_wire_member_and_positional_argument()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (_service, client, state, _) = fixture().await?;
    let samples = [1.0_f32, -1.0];
    let gates = tinyvoice_bus::RecordingGates {
        min_duration_secs: 0.3,
        silence_threshold: 0.01,
    };

    assert_eq!(client.route("pause").await?, tinyvoice_bus::VoiceIntent::Pause);
    assert_eq!(
        client.sequence_utterance("hey tiny open slack", "Hey Tiny").await?,
        tinyvoice_bus::UtteranceDisposition::Command {
            command: "open slack".to_owned()
        }
    );
    assert_eq!(
        client.gate_recording(1.0, 0.5, gates).await?,
        tinyvoice_bus::GateVerdict::Pass
    );
    assert!(client
        .is_hallucinated("okay", tinyvoice_bus::Mode::Dictation)
        .await?);
    assert_eq!(client.prepare_capture(&samples, 32_000, 2, 0.0).await?, [1]);
    assert_eq!(client.encode_wav(&samples, 16_000).await?, [1]);
    assert_eq!(client.prepare_frames(&samples, 32_000, 2).await?, [0.0]);
    assert_eq!(client.frame_energies(&samples, 2).await?, [0.25]);
    assert_eq!(client.encode_wav_pcm16(&[-1_i16, 2], 16_000, 1).await?, [1]);

    assert_eq!(
        recorded(&state),
        [
            (tinyvoice_bus::names::methods::ROUTE.to_owned(), json!(["pause"])),
            (
                tinyvoice_bus::names::methods::SEQUENCE_UTTERANCE.to_owned(),
                json!(["hey tiny open slack", "Hey Tiny"])
            ),
            (
                tinyvoice_bus::names::methods::GATE_RECORDING.to_owned(),
                json!([1.0, 0.5, 0.3, 0.01])
            ),
            (
                tinyvoice_bus::names::methods::IS_HALLUCINATED.to_owned(),
                json!(["okay", "dictation"])
            ),
            (
                tinyvoice_bus::names::methods::PREPARE_CAPTURE.to_owned(),
                json!([encoded(&samples), 32_000, 2, 0.0])
            ),
            (
                tinyvoice_bus::names::methods::ENCODE_WAV.to_owned(),
                json!([encoded(&samples), 16_000])
            ),
            (
                tinyvoice_bus::names::methods::PREPARE_FRAMES.to_owned(),
                json!([encoded(&samples), 32_000, 2])
            ),
            (
                tinyvoice_bus::names::methods::FRAME_ENERGIES.to_owned(),
                json!([encoded(&samples), 2])
            ),
            (
                tinyvoice_bus::names::methods::ENCODE_WAV_PCM16.to_owned(),
                json!(["/////wIAAA==", 16_000, 1])
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn vad_and_recorder_handles_route_each_operation_and_close_only_explicitly()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (_service, client, state, _) = fixture().await?;
    let vad_config = tinyvoice_bus::VadConfig::default();
    let vad = VadSession::open(&client, vad_config).await?;
    assert!(vad.push(20, &[0.2, 0.0]).await?.is_empty());
    assert!(vad.is_speaking().await?);
    vad.reset().await?;
    let vad_id = vad.id;
    vad.close().await?;
    drop(vad);

    let recorder = RecorderSession::open(&client).await?;
    assert_eq!(
        recorder
            .advance(tinyvoice_bus::SessionInput::Run)
            .await?,
        tinyvoice_bus::SessionAdvance {
            accepted: true,
            phase: tinyvoice_bus::SessionPhase::Idle,
            generation: 0,
            action: tinyvoice_bus::SessionAction::None,
        }
    );
    assert_eq!(
        recorder.status().await?,
        tinyvoice_bus::SessionStatus {
            phase: tinyvoice_bus::SessionPhase::Idle,
            generation: 0,
        }
    );
    let recorder_id = recorder.id();
    recorder.close().await?;
    drop(recorder);

    let abandoned_vad = VadSession::open(&client, vad_config).await?;
    drop(abandoned_vad);
    let abandoned_recorder = RecorderSession::open(&client).await?;
    drop(abandoned_recorder);

    let calls = recorded(&state);
    assert_eq!(
        calls,
        [
            (
                tinyvoice_bus::names::methods::VAD_OPEN.to_owned(),
                json!([r#"{"onset_threshold":0.01,"hangover_ms":800,"min_speech_ms":300,"max_utterance_ms":30000}"#])
            ),
            (
                tinyvoice_bus::names::methods::VAD_PUSH.to_owned(),
                json!([vad_id, 20, [0.2, 0.0]])
            ),
            (
                tinyvoice_bus::names::methods::VAD_IS_SPEAKING.to_owned(),
                json!([vad_id])
            ),
            (tinyvoice_bus::names::methods::VAD_RESET.to_owned(), json!([vad_id])),
            (tinyvoice_bus::names::methods::VAD_CLOSE.to_owned(), json!([vad_id])),
            (tinyvoice_bus::names::methods::SESSION_OPEN.to_owned(), json!([])),
            (
                tinyvoice_bus::names::methods::SESSION_ADVANCE.to_owned(),
                json!([recorder_id, r#"{"input":"run"}"#])
            ),
            (
                tinyvoice_bus::names::methods::SESSION_STATUS.to_owned(),
                json!([recorder_id])
            ),
            (
                tinyvoice_bus::names::methods::SESSION_CLOSE.to_owned(),
                json!([recorder_id])
            ),
            (tinyvoice_bus::names::methods::VAD_OPEN.to_owned(), json!([r#"{"onset_threshold":0.01,"hangover_ms":800,"min_speech_ms":300,"max_utterance_ms":30000}"#])),
            (tinyvoice_bus::names::methods::SESSION_OPEN.to_owned(), json!([])),
        ]
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(member, _)| member == tinyvoice_bus::names::methods::VAD_CLOSE)
            .count(),
        1,
        "dropping an open VAD handle must not invent a remote close"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(member, _)| member == tinyvoice_bus::names::methods::SESSION_CLOSE)
            .count(),
        1,
        "dropping an open recorder handle must not invent a remote close"
    );
    Ok(())
}

#[tokio::test]
async fn capture_and_hotkey_wrappers_preserve_typed_requests_and_business_errors()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (_service, client, state, _) = fixture().await?;
    let permission = tinyvoice_bus::MicrophonePermission::Granted;
    let capture = client.reserve_capture(permission).await?.expect("capture reservation");
    let stream = client
        .start_capture(capture.clone(), permission)
        .await?
        .expect("capture stream");
    let poll = tinyvoice_bus::CapturePollRequest {
        handle: stream.handle.clone(),
        max_chunks: 2,
    };
    assert!(client.poll_capture(poll.clone()).await?.expect("poll").chunks.is_empty());
    client.stop_capture(stream.handle.clone()).await?;
    client.shutdown_capture().await?;

    let request = tinyvoice_bus::HotkeyRequest {
        key: "space".to_owned(),
        mode: tinyvoice_bus::ActivationMode::Push,
        source: tinyvoice_bus::HotkeySource::Host,
    };
    let handle = client
        .reserve_hotkey(request.clone())
        .await?
        .expect("hotkey reservation");
    let started = client.start_hotkey(handle.clone()).await?.expect("hotkey start");
    let read = tinyvoice_bus::HotkeyReadRequest {
        handle: handle.clone(),
        acknowledged_batch: None,
    };
    assert!(client.read_hotkey(read.clone()).await?.expect("hotkey read").events.is_empty());
    let feed = tinyvoice_bus::HotkeyFeedRequest {
        handle: handle.clone(),
        generation: started.generation.expect("started generation"),
        facts: vec![tinyvoice_bus::SequencedHostFact {
            sequence: 1,
            fact: tinyvoice_bus::HostKeyFact::Down,
        }],
        overflow: false,
    };
    assert_eq!(client.feed_hotkey(feed.clone()).await?.expect("host feed").state, started.state);
    client.stop_hotkey(handle.clone()).await?.expect("hotkey stop");
    client.shutdown_hotkeys().await?.expect("hotkey shutdown");

    let denied_reply: Value = json!({"Err": "permission_denied"});
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replies
        .push((tinyvoice_bus::names::methods::HOTKEY_RESERVE.to_owned(), denied_reply));
    assert_eq!(
        client.reserve_hotkey(request.clone()).await?,
        Err(tinyvoice_bus::HotkeyError::PermissionDenied)
    );

    let calls = recorded(&state);
    assert_eq!(
        calls,
        [
            (
                tinyvoice_bus::names::methods::RESERVE_CAPTURE.to_owned(),
                json!([{"permission":"granted","handle":null}])
            ),
            (
                tinyvoice_bus::names::methods::CAPTURE_START.to_owned(),
                json!([{"permission":"granted","handle":"capture-1"}])
            ),
            (
                tinyvoice_bus::names::methods::CAPTURE_POLL.to_owned(),
                json!([{"handle":"capture-1","max_chunks":2}])
            ),
            (
                tinyvoice_bus::names::methods::CAPTURE_STOP.to_owned(),
                json!([{"handle":"capture-1"}])
            ),
            (tinyvoice_bus::names::methods::CAPTURE_SHUTDOWN.to_owned(), json!([])),
            (
                tinyvoice_bus::names::methods::HOTKEY_RESERVE.to_owned(),
                json!([{"request":{"key":"space","mode":"push","source":"host"}}])
            ),
            (
                tinyvoice_bus::names::methods::HOTKEY_START.to_owned(),
                json!([{"handle":"hotkey-1"}])
            ),
            (
                tinyvoice_bus::names::methods::HOTKEY_READ.to_owned(),
                json!([{"handle":"hotkey-1","acknowledged_batch":null}])
            ),
            (
                tinyvoice_bus::names::methods::HOTKEY_FEED.to_owned(),
                json!([{"handle":"hotkey-1","generation":1,"facts":[{"sequence":1,"fact":"down"}],"overflow":false}])
            ),
            (
                tinyvoice_bus::names::methods::HOTKEY_STOP.to_owned(),
                json!([{"handle":"hotkey-1"}])
            ),
            (tinyvoice_bus::names::methods::HOTKEY_SHUTDOWN.to_owned(), json!([])),
            (
                tinyvoice_bus::names::methods::HOTKEY_RESERVE.to_owned(),
                json!([{"request":{"key":"space","mode":"push","source":"host"}}])
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn unexpected_intents_and_module_errors_remain_closed_and_redacted()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (_service, client, state, failures) = fixture().await?;
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replies
        .push((
            tinyvoice_bus::names::methods::ROUTE.to_owned(),
            json!(r#"{"intent":"newer_unrecognized_intent"}"#),
        ));
    assert_eq!(client.route("unrecognized command").await?, tinyvoice_bus::VoiceIntent::Unknown);

    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .failure = Some(FailureMode::Unavailable);
    let unavailable = client.route("secret transcript").await.unwrap_err();
    assert_eq!(unavailable, ModuleCallError::Unavailable);
    assert_eq!(format!("{unavailable:?}"), "Unavailable");

    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .failure = Some(FailureMode::Execution);
    let failure = client.route("secret transcript").await.unwrap_err();
    assert_eq!(failure, ModuleCallError::ModuleFault);
    assert_eq!(format!("{failure:?}"), "ModuleFault");
    assert!(!failure.to_string().contains("sensitive transcript marker"));

    let reports = failures
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0], (MODULE_ID.to_owned(), "module_unavailable".to_owned()));
    assert_eq!(reports[1], (MODULE_ID.to_owned(), "module_fault".to_owned()));
    assert!(!format!("{reports:?}").contains("sensitive transcript marker"));
    Ok(())
}
