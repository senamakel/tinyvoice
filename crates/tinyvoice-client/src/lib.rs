//! Typed host calls to the `TinyVoice` `TinyBus` module.
//!
//! The client owns the method names, positional payloads, and conversions at
//! the module boundary. Hosts provide one configured
//! [`ModuleClient`](tinybus::module::ModuleClient); loading, deadlines, and
//! terminal reporting remain `TinyBus` responsibilities.

use std::fmt;

use serde::{Serialize, de::DeserializeOwned};
use tinybus::module::{ModuleCallError, ModuleCallFailure, ModuleClient};

pub use tinyvoice_bus as contract;
pub use tinyvoice_bus::capture::{
    CaptureBatch, CaptureError, CaptureFormat, CaptureHandle, CapturePollRequest, CaptureResult,
    CaptureStream, MicrophonePermission, RawChunk, RecordingFinishRequest, RecordingStartRequest,
};
pub use tinyvoice_bus::hotkey::{
    ActivationMode, HostKeyFact, HotkeyBatch, HotkeyError, HotkeyEvent, HotkeyFeedRequest,
    HotkeyHandle, HotkeyHandleRequest, HotkeyReadRequest, HotkeyReply, HotkeyRequest,
    HotkeyReserveRequest, HotkeySource, HotkeyStartRequest, HotkeyState, SequencedHostFact,
};
pub use tinyvoice_bus::transcript::Mode as HallucinationMode;
pub use tinyvoice_bus::{
    GateVerdict, IndexedVadEvent, RecordingGates, SessionAction, SessionAdvance, SessionInput,
    SessionPhase, SessionStatus, UtteranceDisposition, VadConfig, VadEvent, VoiceIntent,
};

/// Registry id of the loadable module these calls use.
pub const MODULE_ID: &str = "tinyvoice";

/// Result of a typed `TinyVoice` operation.
pub type Result<T> = std::result::Result<T, VoiceCallError>;

/// A terminal `TinyBus` failure with its untrusted remote prose kept private by
/// default.
///
/// `Debug`, `Display`, and `TinyBus` terminal reports contain only the closed
/// error classification. A caller that has an explicit user-facing policy may
/// retrieve remote prose through [`raw_message`](Self::raw_message); it must
/// not log that value or forward it to telemetry without redaction.
#[derive(Clone, PartialEq, Eq)]
pub struct VoiceCallError {
    kind: ModuleCallError,
    wire_name: String,
    message: String,
}

impl VoiceCallError {
    fn from_failure(failure: ModuleCallFailure) -> Self {
        Self {
            kind: failure.kind,
            wire_name: failure.wire_name,
            message: failure.message,
        }
    }

    fn local(kind: ModuleCallError, wire_name: &'static str, message: String) -> Self {
        Self {
            kind,
            wire_name: wire_name.to_owned(),
            message,
        }
    }

    /// Return the closed error category without exposing remote prose.
    #[must_use]
    pub const fn kind(&self) -> ModuleCallError {
        self.kind
    }

    /// Return the remote wire error name for an explicitly handled failure.
    ///
    /// This value is untrusted remote input. Do not log or report it without
    /// applying a redaction policy.
    #[must_use]
    pub fn raw_wire_name(&self) -> &str {
        &self.wire_name
    }

    /// Return the remote error prose for an explicitly handled failure.
    ///
    /// This value can contain call arguments or credentials. Do not log or
    /// report it without applying a redaction policy.
    #[must_use]
    pub fn raw_message(&self) -> &str {
        &self.message
    }
}

impl fmt::Debug for VoiceCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VoiceCallError")
            .field("kind", &self.kind)
            .field("wire_name", &"<redacted>")
            .field("message", &"<redacted>")
            .finish()
    }
}

impl fmt::Display for VoiceCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(formatter)
    }
}

impl std::error::Error for VoiceCallError {}

/// Typed `TinyVoice` calls over one host-configured `TinyBus` module client.
#[derive(Clone)]
pub struct TinyVoiceClient {
    module: ModuleClient,
}

impl fmt::Debug for TinyVoiceClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TinyVoiceClient")
            .field("module_id", &MODULE_ID)
            .finish_non_exhaustive()
    }
}

impl TinyVoiceClient {
    /// Wrap the host's configured `TinyBus` client.
    ///
    /// This stores the given client and does not open a connection or create a
    /// second resolver. Calls use its module access, reporting sink, fixture,
    /// and configured deadline.
    #[must_use]
    pub fn new(module: ModuleClient) -> Self {
        Self { module }
    }

    /// Classify a wake-word-stripped transcript into a fast-path intent.
    ///
    /// Unknown intent variants from a newer compatible module decode to
    /// [`VoiceIntent::Unknown`].
    ///
    /// # Errors
    ///
    /// Returns the sanitized `TinyBus` failure category. Its raw remote details
    /// are available only through the explicit getters on [`VoiceCallError`].
    pub async fn route(&self, transcript: &str) -> Result<VoiceIntent> {
        self.call_json(
            tinyvoice_bus::names::methods::ROUTE,
            (transcript,),
            "intent",
        )
        .await
    }

    /// Sequence a transcript through the module's wake-word gate.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure or a sanitized malformed-reply
    /// error.
    pub async fn sequence_utterance(
        &self,
        transcript: &str,
        wake_word: &str,
    ) -> Result<UtteranceDisposition> {
        self.call_json(
            tinyvoice_bus::names::methods::SEQUENCE_UTTERANCE,
            (transcript, wake_word),
            "utterance disposition",
        )
        .await
    }

    /// Apply the module's minimum-duration and silence gates.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure or a sanitized malformed-reply
    /// error.
    pub async fn gate_recording(
        &self,
        duration_secs: f32,
        peak_rms: f32,
        gates: RecordingGates,
    ) -> Result<GateVerdict> {
        self.call_json(
            tinyvoice_bus::names::methods::GATE_RECORDING,
            (
                duration_secs,
                peak_rms,
                gates.min_duration_secs,
                gates.silence_threshold,
            ),
            "recording gate verdict",
        )
        .await
    }

    /// Check whether a transcript is a likely speech-recognition hallucination.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure.
    pub async fn is_hallucinated(&self, text: &str, mode: HallucinationMode) -> Result<bool> {
        self.call(
            tinyvoice_bus::names::methods::IS_HALLUCINATED,
            (text, hallucination_mode_wire(mode)),
        )
        .await
    }

    /// Downmix, resample, optionally silence-gate, and frame samples as WAV.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure or malformed audio-reply error.
    pub async fn prepare_capture(
        &self,
        samples: &[f32],
        source_rate: u32,
        channels: u16,
        gate_threshold: f32,
    ) -> Result<Vec<u8>> {
        let wav: String = self
            .call(
                tinyvoice_bus::names::methods::PREPARE_CAPTURE,
                (
                    encode_samples(samples),
                    source_rate,
                    channels,
                    gate_threshold,
                ),
            )
            .await?;
        self.decode_audio(&wav)
    }

    /// Frame mono `f32` samples as a 16-bit PCM WAV file.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure or malformed audio-reply error.
    pub async fn encode_wav(&self, samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
        let wav: String = self
            .call(
                tinyvoice_bus::names::methods::ENCODE_WAV,
                (encode_samples(samples), sample_rate),
            )
            .await?;
        self.decode_audio(&wav)
    }

    /// Downmix and resample an interleaved buffer to 16 kHz mono samples.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure or malformed sample-reply error.
    pub async fn prepare_frames(
        &self,
        samples: &[f32],
        source_rate: u32,
        channels: u16,
    ) -> Result<Vec<f32>> {
        let encoded: String = self
            .call(
                tinyvoice_bus::names::methods::PREPARE_FRAMES,
                (encode_samples(samples), source_rate, channels),
            )
            .await?;
        self.decode_samples(&encoded)
    }

    /// Calculate RMS energy for each fixed-size sample frame.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure.
    pub async fn frame_energies(&self, samples: &[f32], frame_len: u32) -> Result<Vec<f32>> {
        self.call(
            tinyvoice_bus::names::methods::FRAME_ENERGIES,
            (encode_samples(samples), frame_len),
        )
        .await
    }

    /// Frame mono 16-bit samples as WAV without a lossy conversion.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure or malformed audio-reply error.
    pub async fn encode_wav_pcm16(
        &self,
        samples: &[i16],
        sample_rate: u32,
        channels: u16,
    ) -> Result<Vec<u8>> {
        use base64::Engine as _;
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let wav: String = self
            .call(
                tinyvoice_bus::names::methods::ENCODE_WAV_PCM16,
                (encoded, sample_rate, channels),
            )
            .await?;
        self.decode_audio(&wav)
    }

    /// Open a module-owned VAD session.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure.
    pub async fn open_vad(&self, config: VadConfig) -> Result<VadSession> {
        let config = encode_json(&config, "VAD config")?;
        let id = self
            .call(tinyvoice_bus::names::methods::VAD_OPEN, (config,))
            .await?;
        Ok(VadSession {
            client: self.clone(),
            id,
        })
    }

    /// Reserve an opaque capture lease before native microphone startup.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a capture refusal remains the
    /// typed inner [`CaptureError`].
    pub async fn reserve_capture(
        &self,
        permission: MicrophonePermission,
    ) -> Result<CaptureResult<CaptureHandle>> {
        self.call(
            tinyvoice_bus::names::methods::RESERVE_CAPTURE,
            (RecordingStartRequest {
                permission,
                handle: None,
            },),
        )
        .await
    }

    /// Start module-owned continuous capture with a known reservation.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a capture refusal remains the
    /// typed inner [`CaptureError`].
    pub async fn start_capture(
        &self,
        handle: CaptureHandle,
        permission: MicrophonePermission,
    ) -> Result<CaptureResult<CaptureStream>> {
        self.call(
            tinyvoice_bus::names::methods::CAPTURE_START,
            (RecordingStartRequest {
                permission,
                handle: Some(handle),
            },),
        )
        .await
    }

    /// Read a bounded batch from a continuous capture lease.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a capture refusal remains the
    /// typed inner [`CaptureError`].
    pub async fn poll_capture(
        &self,
        request: CapturePollRequest,
    ) -> Result<CaptureResult<CaptureBatch>> {
        self.call(tinyvoice_bus::names::methods::CAPTURE_POLL, (request,))
            .await
    }

    /// Stop capture and wait for native resource cleanup.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a capture refusal remains the
    /// typed inner [`CaptureError`].
    pub async fn stop_capture(&self, handle: CaptureHandle) -> Result<CaptureResult<()>> {
        self.call(tinyvoice_bus::names::methods::CAPTURE_STOP, (handle,))
            .await
    }

    /// Shut down every module-owned capture lease.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a capture refusal remains the
    /// typed inner [`CaptureError`].
    pub async fn shutdown_capture(&self) -> Result<CaptureResult<()>> {
        self.call(tinyvoice_bus::names::methods::CAPTURE_SHUTDOWN, ())
            .await
    }

    /// Reserve a native or host-fed hotkey listener.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a listener refusal remains the
    /// typed inner [`HotkeyError`].
    pub async fn reserve_hotkey(
        &self,
        request: HotkeyRequest,
    ) -> Result<std::result::Result<HotkeyHandle, HotkeyError>> {
        self.call(
            tinyvoice_bus::names::methods::HOTKEY_RESERVE,
            (HotkeyReserveRequest { request },),
        )
        .await
    }

    /// Start a reserved listener, preserving the module's typed result.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a listener refusal remains the
    /// typed inner [`HotkeyError`].
    pub async fn start_hotkey(
        &self,
        handle: HotkeyHandle,
    ) -> Result<std::result::Result<HotkeyReply, HotkeyError>> {
        self.call(
            tinyvoice_bus::names::methods::HOTKEY_START,
            (HotkeyStartRequest { handle },),
        )
        .await
    }

    /// Read an acknowledged/replayable activation batch.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a listener refusal remains the
    /// typed inner [`HotkeyError`].
    pub async fn read_hotkey(
        &self,
        request: HotkeyReadRequest,
    ) -> Result<std::result::Result<HotkeyBatch, HotkeyError>> {
        self.call(tinyvoice_bus::names::methods::HOTKEY_READ, (request,))
            .await
    }

    /// Feed a bounded batch of host-owned generic key facts.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a listener refusal remains the
    /// typed inner [`HotkeyError`].
    pub async fn feed_hotkey(
        &self,
        request: HotkeyFeedRequest,
    ) -> Result<std::result::Result<HotkeyReply, HotkeyError>> {
        self.call(tinyvoice_bus::names::methods::HOTKEY_FEED, (request,))
            .await
    }

    /// Stop a listener after native cleanup has completed.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a listener refusal remains the
    /// typed inner [`HotkeyError`].
    pub async fn stop_hotkey(
        &self,
        handle: HotkeyHandle,
    ) -> Result<std::result::Result<HotkeyReply, HotkeyError>> {
        self.call(
            tinyvoice_bus::names::methods::HOTKEY_STOP,
            (HotkeyHandleRequest { handle },),
        )
        .await
    }

    /// Shut down every module-owned hotkey listener.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure; a listener refusal remains the
    /// typed inner [`HotkeyError`].
    pub async fn shutdown_hotkeys(&self) -> Result<std::result::Result<HotkeyReply, HotkeyError>> {
        self.call(tinyvoice_bus::names::methods::HOTKEY_SHUTDOWN, ())
            .await
    }

    async fn call<A, R>(&self, method: &str, args: A) -> Result<R>
    where
        A: Serialize + Send,
        R: DeserializeOwned,
    {
        self.module
            .call_detailed(MODULE_ID, method, args)
            .await
            .map_err(VoiceCallError::from_failure)
    }

    async fn call_json<A, R>(&self, method: &str, args: A, field: &str) -> Result<R>
    where
        A: Serialize + Send,
        R: DeserializeOwned,
    {
        let response: String = self.call(method, args).await?;
        serde_json::from_str(&response)
            .map_err(|error| self.malformed_reply(format!("could not decode {field}: {error}")))
    }

    fn decode_audio(&self, encoded: &str) -> Result<Vec<u8>> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| {
                self.malformed_reply(format!("module returned invalid audio: {error}"))
            })
    }

    fn decode_samples(&self, encoded: &str) -> Result<Vec<f32>> {
        let bytes = self.decode_audio(encoded)?;
        if !bytes.len().is_multiple_of(4) {
            return Err(self.malformed_reply(format!(
                "module returned {} audio bytes, not a whole number of samples",
                bytes.len()
            )));
        }
        let (samples, _) = bytes.as_chunks::<4>();
        Ok(samples.iter().copied().map(f32::from_le_bytes).collect())
    }

    fn malformed_reply(&self, message: String) -> VoiceCallError {
        self.module.report_malformed_reply(MODULE_ID);
        VoiceCallError::local(
            ModuleCallError::ModuleFault,
            "ai.tinyhumans.tinybus.Error.BadArguments",
            message,
        )
    }
}

/// A module-owned VAD session. Its opaque id is omitted from debug output.
///
/// The handle is an explicit async lease: call [`close`](Self::close) when it
/// is no longer needed. Dropping it does not perform remote cleanup.
#[derive(Clone)]
pub struct VadSession {
    client: TinyVoiceClient,
    id: u64,
}

impl fmt::Debug for VadSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VadSession")
            .field("id", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl VadSession {
    /// Open a session with the supplied VAD tuning.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure.
    pub async fn open(client: &TinyVoiceClient, config: VadConfig) -> Result<Self> {
        client.open_vad(config).await
    }

    /// Push frame energies and return the events emitted for this batch.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure or malformed-reply error.
    pub async fn push(&self, frame_ms: u32, energies: &[f32]) -> Result<Vec<IndexedVadEvent>> {
        self.client
            .call_json(
                tinyvoice_bus::names::methods::VAD_PUSH,
                (self.id, frame_ms, energies),
                "VAD events",
            )
            .await
    }

    /// Report whether the session is currently inside an utterance.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure.
    pub async fn is_speaking(&self) -> Result<bool> {
        self.client
            .call(tinyvoice_bus::names::methods::VAD_IS_SPEAKING, (self.id,))
            .await
    }

    /// Drop any in-flight utterance without emitting an event.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure.
    pub async fn reset(&self) -> Result<()> {
        self.client
            .call(tinyvoice_bus::names::methods::VAD_RESET, (self.id,))
            .await
    }

    /// Release the session. Dropping the Rust handle alone does not close it.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure.
    pub async fn close(&self) -> Result<()> {
        self.client
            .call(tinyvoice_bus::names::methods::VAD_CLOSE, (self.id,))
            .await
    }
}

/// A module-owned hotkey recording session. Its opaque id is omitted from
/// debug output.
///
/// This is an explicit async lease; call [`close`](Self::close) when it is no
/// longer needed. Dropping it does not perform remote cleanup.
#[derive(Clone)]
pub struct RecorderSession {
    client: TinyVoiceClient,
    id: u64,
}

impl fmt::Debug for RecorderSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecorderSession")
            .field("id", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl RecorderSession {
    /// Open a stopped recording session.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure.
    pub async fn open(client: &TinyVoiceClient) -> Result<Self> {
        let id = client
            .call(tinyvoice_bus::names::methods::SESSION_OPEN, ())
            .await?;
        Ok(Self {
            client: client.clone(),
            id,
        })
    }

    /// Apply an input to the recording state machine.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure or malformed-reply error.
    pub async fn advance(&self, input: SessionInput) -> Result<SessionAdvance> {
        let encoded = encode_json(&input, "recording session input")?;
        self.client
            .call_json(
                tinyvoice_bus::names::methods::SESSION_ADVANCE,
                (self.id, encoded),
                "recording session advance",
            )
            .await
    }

    /// Read the session's phase and latest generation.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure or malformed-reply error.
    pub async fn status(&self) -> Result<SessionStatus> {
        self.client
            .call_json(
                tinyvoice_bus::names::methods::SESSION_STATUS,
                (self.id,),
                "recording session status",
            )
            .await
    }

    /// Release the session. Dropping the Rust handle alone does not close it.
    ///
    /// # Errors
    ///
    /// Returns a sanitized `TinyBus` failure.
    pub async fn close(&self) -> Result<()> {
        self.client
            .call(tinyvoice_bus::names::methods::SESSION_CLOSE, (self.id,))
            .await
    }
}

fn encode_json<T: Serialize>(value: &T, label: &str) -> Result<String> {
    serde_json::to_string(value).map_err(|error| {
        VoiceCallError::local(
            ModuleCallError::ModuleFault,
            "ai.tinyhumans.tinybus.Error.BadArguments",
            format!("could not encode {label}: {error}"),
        )
    })
}

fn encode_samples(samples: &[f32]) -> String {
    use base64::Engine as _;
    let bytes: Vec<u8> = samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn hallucination_mode_wire(mode: HallucinationMode) -> &'static str {
    match mode {
        HallucinationMode::Dictation => "dictation",
        HallucinationMode::Conversation => "conversation",
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
