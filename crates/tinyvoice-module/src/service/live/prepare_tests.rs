//! Explicit native configuration and bounded display-metadata fixtures.

use super::*;
use serde_json::json;
use tinybus::broker::Broker;
use tinybus::transport::memory::MemoryBus;

async fn fixture() -> anyhow::Result<(LiveStartRequest, tinybus::Connection)> {
    let bus = MemoryBus::new();
    Broker::new().spawn(bus.clone());
    Ok((
        serde_json::from_str(include_str!("fixtures/start.json"))?,
        tinybus::Connection::connect(bus.connect().await?).await?,
    ))
}

#[tokio::test]
async fn oversized_display_identity_cannot_create_an_unreadable_ready_batch() -> anyhow::Result<()>
{
    let (mut request, connection) = fixture().await?;
    request.thread_id = "\0".repeat(16 * 1024 + 1);
    assert!(prepare(&request, &connection).is_err());
    Ok(())
}

#[tokio::test]
async fn native_configuration_preserves_all_explicit_host_fields() -> anyhow::Result<()> {
    let (request, connection) = fixture().await?;
    let prepared = prepare(&request, &connection)?;
    assert_eq!(prepared.config.model, request.configuration.model);
    assert_eq!(
        prepared.config.system_instruction,
        request.configuration.system_instruction
    );
    assert_eq!(prepared.config.voice, request.configuration.voice);
    assert_eq!(prepared.config.language, request.configuration.language);
    assert_eq!(
        prepared.config.first_message,
        request.configuration.first_message
    );
    assert_eq!(prepared.config.input_format, AudioFormat::pcm16(16000));
    assert!(prepared.config.input_transcription && prepared.config.output_transcription);
    assert_eq!(prepared.config.vad.turn_detection, TurnDetection::Manual);
    assert_eq!(prepared.config.vad.silence_ms, Some(600));
    assert_eq!(prepared.config.provider_options, json!({"host": true}));
    assert_eq!(
        prepared.options.tool_timeout,
        std::time::Duration::from_secs(1)
    );
    assert!(!prepared.options.include_deferred_tools);
    Ok(())
}

#[tokio::test]
async fn provider_construction_uses_only_host_supplied_connection_material() -> anyhow::Result<()> {
    let (mut request, connection) = fixture().await?;
    for provider in [
        LiveConnection::Gemini {
            api_key: "fixture".into(),
        },
        LiveConnection::ElevenLabs {
            signed_url: "ws://127.0.0.1:9".into(),
        },
        LiveConnection::Sarvam {
            api_key: "fixture".into(),
        },
    ] {
        request.connection = provider;
        request.configuration.manual_turn_detection = false;
        assert!(prepare(&request, &connection).is_ok());
    }
    Ok(())
}

#[tokio::test]
async fn empty_or_duplicate_declarations_never_create_a_dispatcher() -> anyhow::Result<()> {
    let (mut request, connection) = fixture().await?;
    let mut declaration = tinyvoice_bus::live::session::LiveToolDeclaration {
        name: String::new(),
        description: "fixture".into(),
        parameters: json!({"type": "object"}),
        prefer_markdown: false,
    };
    request.configuration.tools = vec![declaration.clone()];
    assert!(prepare(&request, &connection).is_err());
    declaration.name = "approved".into();
    request.configuration.tools = vec![declaration.clone(), declaration];
    assert!(prepare(&request, &connection).is_err());
    request.configuration.tools.pop();
    let prepared = prepare(&request, &connection)?;
    assert_eq!(prepared.config.tools.len(), 1);
    assert_eq!(prepared.config.tools[0].name, "approved");
    Ok(())
}

#[tokio::test]
async fn host_timeout_and_tool_call_limits_are_finite_and_positive() -> anyhow::Result<()> {
    let (request, connection) = fixture().await?;
    for (tool_ms, connect_ms, calls) in [
        (0, 1, 1),
        (1, 0, 1),
        (1, 1, 0),
        (1, 1, 10001),
        (1, 60001, 1),
        (1_800_001, 1, 1),
    ] {
        let mut invalid = request.clone();
        invalid.tool_timeout_ms = tool_ms;
        invalid.connect_timeout_ms = connect_ms;
        invalid.max_tool_calls = calls;
        assert!(prepare(&invalid, &connection).is_err());
    }
    Ok(())
}
