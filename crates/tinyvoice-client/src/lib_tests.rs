//! Product-facing wire tests over a real in-memory TinyBus broker.

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

struct MockVoice {
    calls: Calls,
}

#[async_trait]
impl Interface for MockVoice {
    fn name(&self) -> InterfaceName {
        InterfaceName::new(tinyvoice_bus::BUS_NAME)
            .unwrap_or_else(|_| std::process::abort())
    }

    fn members(&self) -> Vec<MemberName> {
        tinyvoice_bus::METHODS
            .iter()
            .filter_map(|name| MemberName::new(*name).ok())
            .collect()
    }

    async fn call(&self, member: &MemberName, args: Value) -> tinybus::Result<Value> {
        let name = member.as_str();
        self.calls
            .lock()
            .map_err(|_| tinybus::Error::failed("fixture call log is poisoned"))?
            .push((name.to_owned(), args));
        let reply = match name {
            tinyvoice_bus::names::methods::ROUTE => json!(r#"{"intent":"pause"}"#),
            tinyvoice_bus::names::methods::VAD_OPEN => json!(41),
            tinyvoice_bus::names::methods::VAD_CLOSE
            | tinyvoice_bus::names::methods::SESSION_CLOSE => Value::Null,
            tinyvoice_bus::names::methods::SESSION_OPEN => json!(73),
            _ => Value::Null,
        };
        Ok(reply)
    }
}

async fn fixture() -> tinybus::Result<(Connection, TinyVoiceClient, Calls)> {
    let bus = MemoryBus::new();
    Broker::new().spawn(bus.clone());
    let service = Connection::connect(bus.connect().await?).await?;
    let calls = Arc::new(Mutex::new(Vec::new()));
    service
        .serve_at(
            ObjectPath::new(tinyvoice_bus::OBJECT_PATH)?,
            MockVoice {
                calls: Arc::clone(&calls),
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
    let module = ModuleClient::new(FixtureAccess {
        reporter: FailureReporter::new(Arc::new(IgnoreFailures)),
    })
    .with_fixture(proxy);
    Ok((service, TinyVoiceClient::new(module), calls))
}

#[tokio::test]
async fn typed_calls_and_session_handles_use_the_recording_broker()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (_service, client, calls) = fixture().await?;

    assert_eq!(client.route("please pause").await?, tinyvoice_bus::VoiceIntent::Pause);

    let vad = VadSession::open(&client, tinyvoice_bus::VadConfig::default()).await?;
    vad.close().await?;
    drop(vad);

    let recorder = RecorderSession::open(&client).await?;
    recorder.close().await?;
    drop(recorder);

    let actual = calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        actual,
        [
            (tinyvoice_bus::names::methods::ROUTE.to_owned(), json!(["please pause"])),
            (
                tinyvoice_bus::names::methods::VAD_OPEN.to_owned(),
                json!([r#"{"onset_threshold":0.01,"hangover_ms":800,"min_speech_ms":300,"max_utterance_ms":30000}"#]),
            ),
            (tinyvoice_bus::names::methods::VAD_CLOSE.to_owned(), json!([41])),
            (tinyvoice_bus::names::methods::SESSION_OPEN.to_owned(), json!([])),
            (tinyvoice_bus::names::methods::SESSION_CLOSE.to_owned(), json!([73])),
        ]
    );
    Ok(())
}
