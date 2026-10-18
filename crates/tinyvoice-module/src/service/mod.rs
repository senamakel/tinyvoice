//! The bus interface, its setup, and the ABI v1 exports.

#[cfg(test)]
#[path = "mod_tests.rs"]
mod test;

mod live;
mod relay;
mod relay_runtime;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tinybus::{Connection, Result as TinyBusResult};
use tinyvoice_bus::{
    HotkeyFeedRequest, HotkeyHandleRequest, HotkeyReadRequest, HotkeyReserveRequest, HotkeyResult,
};
use tinyvoice_bus::{IndexedVadEvent, SessionInput, names};

use tinyvoice::audio::{self, SilenceGateConfig};
use tinyvoice::intent;
use tinyvoice::session::HotkeyRecorder;
use tinyvoice::transcript::{self, Mode};
use tinyvoice::vad::{VadConfig, VadSegmenter};

/// Largest audio payload accepted or produced in a single call.
///
/// A bus frame caps at 16 MiB and base64 costs 1.34x, so this leaves ample room
/// for the JSON envelope around the payload. In wall-clock terms it is roughly
/// eight minutes of 16 kHz mono PCM — far longer than the utterances this path
/// exists to carry, and short enough that a caller cannot turn one frame into
/// an allocation failure.
pub const MAX_AUDIO_BYTES: usize = 8 * 1024 * 1024;

/// Most sessions a caller may hold open at once.
///
/// A session is opened by `VadOpen` and only released by `VadClose`, and a
/// method receives no caller identity — so there is nobody to attribute a leak
/// to and nothing to clean up when a caller goes away. The cap is what stops a
/// host that forgets to close from growing this map without bound; it refuses
/// the *new* session rather than evicting an old one, because evicting would
/// silently truncate an utterance somebody is still recording.
///
/// Generous relative to real use: a host runs one always-on loop.
pub const MAX_SESSIONS: usize = 64;

/// Voice operations served by the compiled module.
///
/// Pure audio, intent and transcript methods have no persistent state. VAD
/// segmenters retain utterance state across frame batches, while capture owns
/// reservations, native recording/stream leases and bounded prepared outputs.
/// Native setup and cleanup run in owned workers; hosts use `CaptureShutdown`
/// before stopping the module and per-handle cleanup when signing out.
#[derive(Debug, Default)]
pub struct VoiceService {
    /// Live segmenters, keyed by the id `VadOpen` handed out.
    ///
    /// A `std::sync::Mutex` rather than an async one: every critical section is
    /// a map lookup and a few integer comparisons, with no await inside, so an
    /// async mutex would add a scheduling hop to a lock that is never contended
    /// for long. It is deliberately never held across an `.await`.
    sessions: std::sync::Mutex<Sessions>,
    capture: std::sync::Arc<capture::Capture>,
    hotkeys: std::sync::Arc<hotkey::Hotkeys>,
    live: std::sync::Arc<live::Sessions>,
    relay: std::sync::Arc<relay_runtime::Turns>,
}

/// The session table and the counter that names its entries.
#[derive(Debug, Default)]
struct Sessions {
    /// Next id to hand out. Monotonic, never reused.
    ///
    /// Reusing a freed id would let a stale handle from one utterance land on
    /// the segmenter of the next, which is the kind of bug that shows up as a
    /// transcript with somebody else's sentence stapled to the front.
    next_id: u64,
    /// Open segmenters.
    live: std::collections::HashMap<u64, VadSegmenter>,
    /// Next recording-session id. Monotonic, never reused, for the same
    /// reason as `next_id`.
    next_recorder_id: u64,
    /// Open hotkey-driven recording sessions, keyed by `SessionOpen`'s id.
    recorders: std::collections::HashMap<u64, HotkeyRecorder>,
}

/// The error for a recording session id that is not open.
fn unknown_recorder(session: u64) -> tinybus::Error {
    tinybus::Error::failed(format!(
        "recording session {session} is not open; it was closed, or never opened in this process"
    ))
}

/// Decode a base64 payload, refusing anything over [`MAX_AUDIO_BYTES`].
///
/// The length is checked *before* decoding. A declared size is a number the
/// caller sent us; allocating for it first and validating after is how a wrong
/// or hostile value becomes an allocation failure, which aborts the process
/// rather than returning an error a caller can see.
fn decode_audio(encoded: &str) -> TinyBusResult<Vec<u8>> {
    // Base64 expands by 4/3, so this bounds the decoded size without decoding.
    if encoded.len() / 4 * 3 > MAX_AUDIO_BYTES {
        return Err(tinybus::Error::failed(format!(
            "audio payload exceeds the {MAX_AUDIO_BYTES} byte limit"
        )));
    }
    BASE64
        .decode(encoded)
        .map_err(|e| tinybus::Error::failed(format!("audio payload is not valid base64: {e}")))
}

/// Bound resampling before allocating its output, including serialized float output.
fn check_expansion(samples: usize, source_rate: u32) -> TinyBusResult<()> {
    if source_rate == 0 {
        return Err(failed(&tinyvoice::Error::ZeroSampleRate));
    }
    let expanded = samples
        .checked_mul(audio::STT_SAMPLE_RATE as usize)
        .map(|count| count.div_ceil(source_rate as usize))
        .ok_or_else(|| tinybus::Error::failed("audio expansion exceeds the output limit"))?;
    if expanded > MAX_AUDIO_BYTES / 4 {
        return Err(tinybus::Error::failed(
            "audio expansion exceeds the output limit",
        ));
    }
    Ok(())
}

/// Reinterpret a little-endian byte buffer as `f32` samples.
fn f32_samples(bytes: &[u8]) -> TinyBusResult<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(tinybus::Error::failed(format!(
            "{} bytes is not a whole number of f32 samples",
            bytes.len()
        )));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&sample| f32::from_le_bytes(sample))
        .collect())
}

/// Map a `tinyvoice` error onto the wire.
fn failed(error: &tinyvoice::Error) -> tinybus::Error {
    tinybus::Error::failed(error.to_string())
}

/// Base64 little-endian `f32`, the shape samples take on this interface.
fn encode_samples(samples: &[f32]) -> String {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    BASE64.encode(bytes)
}

/// The error for a session id that is not open.
///
/// Its own function so the wording is identical across the four methods that
/// can produce it — a host matching on the message should not have to care
/// which call it came from.
fn unknown_session(session: u64) -> tinybus::Error {
    tinybus::Error::failed(format!(
        "VAD session {session} is not open; it was closed, or never opened in this process"
    ))
}

impl VoiceService {
    /// Take the session lock, turning a poisoned mutex into a wire error.
    ///
    /// A poisoned lock means a previous call panicked while holding it. The
    /// panic was already caught by tinybus rather than taking the host down,
    /// but the map may be inconsistent, so this refuses rather than reaching
    /// into it — and says so, because "poisoned" is a fact worth seeing in a
    /// log rather than a generic failure.
    fn lock_sessions(&self) -> TinyBusResult<std::sync::MutexGuard<'_, Sessions>> {
        self.sessions.lock().map_err(|_| {
            tinybus::Error::failed(
                "the VAD session table is poisoned by an earlier panic; restart the host"
                    .to_string(),
            )
        })
    }
}

/// Serialise a value that is known to be representable as JSON.
///
/// The inputs are this crate's own plain data types, so a failure here would be
/// a bug in the type rather than anything a caller did — but it is still
/// reported rather than unwrapped, because a module that panics takes the
/// host's address space with it.
fn to_json<T: serde::Serialize>(value: &T) -> TinyBusResult<String> {
    serde_json::to_string(value)
        .map_err(|e| tinybus::Error::failed(format!("could not encode result: {e}")))
}

// Pure audio operations use required async signatures; device startup runs
// on blocking workers and lifecycle completion awaits native cleanup.
#[allow(
    clippy::unused_async,
    reason = "TinyBus interface methods require async signatures"
)]
#[tinybus::interface(name = "ai.tinyhumans.tinyvoice.Voice")]
impl VoiceService {
    /// Fix explicit host scheduling bounds for this module instance.
    async fn relay_configure(
        &self,
        limits: tinyvoice_bus::relay::runtime::RelayLimits,
    ) -> TinyBusResult<tinyvoice_bus::relay::runtime::RelayLimits> {
        self.relay.configure(limits)
    }
    /// Reserve a bounded turn before native local-brain startup.
    async fn relay_reserve(&self) -> TinyBusResult<tinyvoice_bus::relay::runtime::RelayStatus> {
        self.relay.reserve()
    }
    /// Start generic timing and progress coordination on a known reservation.
    async fn relay_start(
        &self,
        request: tinyvoice_bus::relay::runtime::RelayStartRequest,
    ) -> TinyBusResult<tinyvoice_bus::relay::runtime::RelayStatus> {
        self.relay.start(request)
    }
    /// Cancel one owned coordinator and notify current host invocation cleanup.
    async fn relay_release(
        &self,
        request: tinyvoice_bus::relay::runtime::RelayHandleRequest,
    ) -> TinyBusResult<tinyvoice_bus::relay::runtime::RelayStatus> {
        self.relay.release(request.handle).await
    }
    /// Close admission and stop all owned relay coordinators.
    async fn relay_shutdown(&self) -> TinyBusResult<()> {
        self.relay.shutdown().await
    }
    /// Reserve a bounded live lease before any provider connection begins.
    async fn live_reserve(&self) -> TinyBusResult<tinyvoice_bus::live::session::LiveStatus> {
        self.live.reserve()
    }
    /// Attach the module-owned socket coordinator before provider startup.
    async fn live_bind_transport(
        &self,
        request: tinyvoice_bus::live::transport::LiveBindTransportRequest,
    ) -> TinyBusResult<tinyvoice_bus::live::session::LiveStatus> {
        self.live.bind_transport(request)
    }
    /// Begin provider startup idempotently on a known reservation.
    async fn live_start(
        &self,
        request: tinyvoice_bus::live::session::LiveStartRequest,
    ) -> TinyBusResult<tinyvoice_bus::live::session::LiveStatus> {
        self.live.start(request)
    }
    /// Forward bounded input to the existing native provider sender.
    async fn live_send(
        &self,
        request: tinyvoice_bus::live::session::LiveSendRequest,
    ) -> TinyBusResult<tinyvoice_bus::live::session::LiveStatus> {
        self.live.send(request).await
    }
    /// Read ordered effects, replaying the current batch until acknowledgement.
    async fn live_read(
        &self,
        request: tinyvoice_bus::live::session::LiveReadRequest,
    ) -> TinyBusResult<tinyvoice_bus::live::session::LiveBatch> {
        self.live.read(request).await
    }
    /// Stop owned tasks and notify host context cleanup within a finite deadline.
    async fn live_release(
        &self,
        request: tinyvoice_bus::live::session::LiveHandleRequest,
    ) -> TinyBusResult<tinyvoice_bus::live::session::LiveStatus> {
        self.live.release(request.handle).await
    }
    /// Permanently close admission and stop all owned live tasks.
    async fn live_shutdown(&self) -> TinyBusResult<()> {
        self.live.shutdown().await
    }
    /// Parse portable live control frames without applying host policy.
    async fn prepare_live_input(
        &self,
        request: tinyvoice_bus::live::PrepareLiveInputRequest,
    ) -> TinyBusResult<tinyvoice_bus::live::PreparedLiveInput> {
        tinyvoice::live::prepare_input(request)
            .map_err(|error| tinybus::Error::failed(error.to_string()))
    }
    /// Prepare provider-relayed text without executing the host agent.
    async fn prepare_relay_turn(
        &self,
        request: tinyvoice_bus::relay::RelayTurnRequest,
    ) -> TinyBusResult<tinyvoice_bus::relay::PreparedRelayTurn> {
        relay::prepare(&request)
    }
    /// Enumerate devices inside the module without opening a recording.
    async fn list_input_devices(
        &self,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<Vec<String>>> {
        let capture = self.capture.clone();
        tokio::task::spawn_blocking(move || capture.devices())
            .await
            .map_err(|_| tinybus::Error::failed("capture worker failed"))
    }
    /// Close capture, cancel pending work and await all native cleanup.
    async fn capture_shutdown(&self) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<()>> {
        Ok(self.capture.shutdown().await)
    }
    /// Obtain a known cancellation handle before beginning native device setup.
    async fn reserve_capture(
        &self,
        request: tinyvoice_bus::capture::RecordingStartRequest,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<tinyvoice_bus::capture::CaptureHandle>>
    {
        Ok(self.capture.reserve(request.permission))
    }
    /// Start native continuous capture after a computer-module permission grant.
    async fn capture_start(
        &self,
        request: tinyvoice_bus::capture::RecordingStartRequest,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<tinyvoice_bus::capture::CaptureStream>>
    {
        match self.capture.start_reserved(request, true).await {
            Ok(capture::Started::Stream(stream)) => Ok(Ok(stream)),
            Err(error) => Ok(Err(error)),
            Ok(capture::Started::Recording(_)) => {
                Ok(Err(tinyvoice_bus::capture::CaptureError::InvalidParameters))
            }
        }
    }
    /// Read at most two native chunks without blocking or per-sample requests.
    async fn capture_poll(
        &self,
        request: tinyvoice_bus::capture::CapturePollRequest,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<tinyvoice_bus::capture::CaptureBatch>>
    {
        Ok(self.capture.stream_poll(&request))
    }
    /// Stop capture, wait for device cleanup and release its lease.
    async fn capture_stop(
        &self,
        handle: tinyvoice_bus::capture::CaptureHandle,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<()>> {
        Ok(self.capture.stream_stop(&handle).await)
    }
    /// Open a native recording after an explicit permission decision.
    async fn recording_start(
        &self,
        request: tinyvoice_bus::capture::RecordingStartRequest,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<tinyvoice_bus::capture::CaptureHandle>>
    {
        match self.capture.start_reserved(request, false).await {
            Ok(capture::Started::Recording(handle)) => Ok(Ok(handle)),
            Err(error) => Ok(Err(error)),
            Ok(capture::Started::Stream(_)) => {
                Ok(Err(tinyvoice_bus::capture::CaptureError::InvalidParameters))
            }
        }
    }
    /// Finish and prepare the recording inside the module.
    async fn recording_finish(
        &self,
        request: tinyvoice_bus::capture::RecordingFinishRequest,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<tinyvoice_bus::capture::AudioOutput>>
    {
        Ok(self.capture.finish(request).await)
    }
    /// Return module-computed metrics without returning the captured samples.
    async fn recording_summary(
        &self,
        request: tinyvoice_bus::capture::RecordingSummaryRequest,
    ) -> TinyBusResult<
        tinyvoice_bus::capture::CaptureResult<tinyvoice_bus::capture::RecordingSummary>,
    > {
        Ok(self.capture.summary(&request))
    }
    /// Stop and discard a native recording.
    async fn recording_cancel(
        &self,
        handle: tinyvoice_bus::capture::CaptureHandle,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<()>> {
        Ok(self.capture.cancel(&handle).await)
    }
    /// Read at most one bounded base64 WAV batch.
    async fn read_audio_output(
        &self,
        request: tinyvoice_bus::capture::ReadAudioRequest,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<String>> {
        Ok(self.capture.read(&request))
    }
    /// Release a held WAV, including when the host abandons it.
    async fn release_audio_output(
        &self,
        handle: tinyvoice_bus::capture::CaptureHandle,
    ) -> TinyBusResult<tinyvoice_bus::capture::CaptureResult<()>> {
        Ok(self.capture.release(&handle))
    }

    /// Classify a command transcript, returning a JSON `VoiceIntent`.
    ///
    /// The transcript should already have had any wake word removed by
    /// [`Self::extract_command`].
    async fn route(&self, transcript: String) -> TinyBusResult<String> {
        to_json(&intent::route(&transcript))
    }

    /// Apply the wake-word gate, returning the command that followed it.
    ///
    /// An empty string means the utterance was not addressed to the agent, or
    /// the wake word arrived with no command after it. Those are the same
    /// outcome for a caller — there is nothing to route either way — so they
    /// share one representation rather than needing a nullable wire type.
    async fn extract_command(
        &self,
        transcript: String,
        wake_word: String,
    ) -> TinyBusResult<String> {
        Ok(intent::extract_command(&transcript, &wake_word).unwrap_or_default())
    }

    /// Whether the wake word appears near the start of a transcript.
    async fn wake_word_present(
        &self,
        transcript: String,
        wake_word: String,
    ) -> TinyBusResult<bool> {
        Ok(intent::wake_word_present(&transcript, &wake_word))
    }

    /// Apply the recording gates, returning a JSON `GateVerdict`.
    async fn gate_recording(
        &self,
        duration_secs: f32,
        peak_rms: f32,
        min_duration_secs: f32,
        silence_threshold: f32,
    ) -> TinyBusResult<String> {
        let gates = tinyvoice_bus::session::RecordingGates {
            min_duration_secs,
            silence_threshold,
        };
        to_json(&tinyvoice::session::gate_recording(
            gates,
            duration_secs,
            peak_rms,
        ))
    }

    /// Sequence a transcript through the wake-word gate, returning a JSON
    /// `UtteranceDisposition`.
    async fn sequence_utterance(
        &self,
        transcript: String,
        wake_word: String,
    ) -> TinyBusResult<String> {
        to_json(&tinyvoice::session::sequence_utterance(
            &transcript,
            &wake_word,
        ))
    }

    /// Whether an STT transcript looks like a hallucination.
    ///
    /// `mode` is `"dictation"` or `"conversation"`. An unrecognised value is an
    /// error rather than a silent fallback: the two modes disagree about
    /// whether a bare "okay" is real speech, so guessing would either delete
    /// legitimate chat replies or leak artefacts into dictated text.
    async fn is_hallucinated(&self, text: String, mode: String) -> TinyBusResult<bool> {
        let mode = match mode.as_str() {
            "dictation" => Mode::Dictation,
            "conversation" => Mode::Conversation,
            other => {
                return Err(tinybus::Error::failed(format!(
                    "unknown hallucination mode `{other}`, expected `dictation` or `conversation`"
                )));
            }
        };
        Ok(transcript::is_hallucinated(&text, mode))
    }

    /// Replay a batch of frame energies through a fresh VAD segmenter.
    ///
    /// `config` is a JSON `VadConfig`. Returns a JSON array of `VadEvent`,
    /// each tagged with the index of the frame that produced it.
    ///
    /// The segmenter starts idle on every call and is dropped afterwards, so a
    /// caller streaming a long recording must submit whole utterances rather
    /// than arbitrary slices — a batch that cuts an utterance in half loses the
    /// open segment. That is the cost of a stateless interface, and it is the
    /// reason a realtime host uses the module's stateful VAD methods instead.
    async fn segment(
        &self,
        config: String,
        frame_ms: u32,
        energies: Vec<f32>,
    ) -> TinyBusResult<String> {
        let config: VadConfig = serde_json::from_str(&config)
            .map_err(|e| tinybus::Error::failed(format!("invalid VAD config: {e}")))?;
        if frame_ms == 0 {
            return Err(tinybus::Error::failed(
                "frame_ms must be greater than zero".to_string(),
            ));
        }

        let mut segmenter = VadSegmenter::new(config);
        let events: Vec<IndexedVadEvent> = energies
            .into_iter()
            .enumerate()
            .filter_map(|(frame, rms)| {
                segmenter
                    .push_frame(rms, frame_ms)
                    .map(|event| IndexedVadEvent { frame, event })
            })
            .collect();
        to_json(&events)
    }

    /// Open a VAD session and return its id.
    ///
    /// `config` is a JSON `VadConfig`. The caller must `VadClose` the id when
    /// the capture loop stops; see [`MAX_SESSIONS`] for what happens if it
    /// does not.
    async fn vad_open(&self, config: String) -> TinyBusResult<u64> {
        let config: VadConfig = serde_json::from_str(&config)
            .map_err(|e| tinybus::Error::failed(format!("invalid VAD config: {e}")))?;

        let mut sessions = self.lock_sessions()?;
        if sessions.live.len() >= MAX_SESSIONS {
            return Err(tinybus::Error::failed(format!(
                "at the {MAX_SESSIONS} open VAD session limit; close a session before opening another"
            )));
        }
        let id = sessions.next_id;
        sessions.next_id += 1;
        sessions.live.insert(id, VadSegmenter::new(config));
        Ok(id)
    }

    /// Push frame energies into an open session.
    ///
    /// Returns a JSON array of `VadEvent`, each tagged with the index of the
    /// frame *within this call* that produced it — not a running total, which
    /// the module would have to track and the caller already knows.
    async fn vad_push(
        &self,
        session: u64,
        frame_ms: u32,
        energies: Vec<f32>,
    ) -> TinyBusResult<String> {
        if frame_ms == 0 {
            return Err(tinybus::Error::failed(
                "frame_ms must be greater than zero".to_string(),
            ));
        }

        let events = {
            let mut sessions = self.lock_sessions()?;
            let segmenter = sessions
                .live
                .get_mut(&session)
                .ok_or_else(|| unknown_session(session))?;
            energies
                .into_iter()
                .enumerate()
                .filter_map(|(frame, rms)| {
                    segmenter
                        .push_frame(rms, frame_ms)
                        .map(|event| IndexedVadEvent { frame, event })
                })
                .collect::<Vec<_>>()
        };
        to_json(&events)
    }

    /// Whether an open session is currently inside an utterance.
    async fn vad_is_speaking(&self, session: u64) -> TinyBusResult<bool> {
        let sessions = self.lock_sessions()?;
        sessions
            .live
            .get(&session)
            .map(VadSegmenter::is_speaking)
            .ok_or_else(|| unknown_session(session))
    }

    /// Abort any in-flight utterance without emitting an event.
    ///
    /// This is the privacy hook: a host calls it when the screen locks or
    /// capture is revoked, and the partial utterance is dropped rather than
    /// completed and transcribed.
    async fn vad_reset(&self, session: u64) -> TinyBusResult<()> {
        let mut sessions = self.lock_sessions()?;
        sessions
            .live
            .get_mut(&session)
            .map(VadSegmenter::reset)
            .ok_or_else(|| unknown_session(session))
    }

    /// Release a session.
    ///
    /// Closing an id that is already gone is **not** an error. A host tearing
    /// down after a failure should not have to track whether it got far enough
    /// to open one, and turning cleanup into something that can itself fail
    /// invites the leak this is meant to prevent.
    async fn vad_close(&self, session: u64) -> TinyBusResult<()> {
        self.lock_sessions()?.live.remove(&session);
        Ok(())
    }

    /// Open a hotkey-driven recording session (stopped) and return its id.
    ///
    /// The caller must `SessionClose` the id; see [`MAX_SESSIONS`].
    async fn session_open(&self) -> TinyBusResult<u64> {
        let mut sessions = self.lock_sessions()?;
        if sessions.recorders.len() >= MAX_SESSIONS {
            return Err(tinybus::Error::failed(format!(
                "at the {MAX_SESSIONS} open recording session limit; close a session before opening another"
            )));
        }
        let id = sessions.next_recorder_id;
        sessions.next_recorder_id += 1;
        sessions.recorders.insert(id, HotkeyRecorder::new());
        Ok(id)
    }

    /// Apply a JSON `SessionInput` to a recording session and return a JSON
    /// `SessionAdvance`: whether it applied, the new phase and generation, and
    /// the one action the host must take.
    async fn session_advance(&self, session: u64, input: String) -> TinyBusResult<String> {
        let input: SessionInput = serde_json::from_str(&input)
            .map_err(|e| tinybus::Error::failed(format!("invalid session input: {e}")))?;
        let reply = {
            let mut sessions = self.lock_sessions()?;
            sessions
                .recorders
                .get_mut(&session)
                .ok_or_else(|| unknown_recorder(session))?
                .advance(input)
        };
        to_json(&reply)
    }

    /// A JSON `SessionStatus`: the session's phase and latest generation.
    async fn session_status(&self, session: u64) -> TinyBusResult<String> {
        let status = {
            let sessions = self.lock_sessions()?;
            sessions
                .recorders
                .get(&session)
                .ok_or_else(|| unknown_recorder(session))?
                .status()
        };
        to_json(&status)
    }

    /// Release a recording session. Closing an id that is already gone is not
    /// an error, for the same reason as `VadClose`.
    async fn session_close(&self, session: u64) -> TinyBusResult<()> {
        self.lock_sessions()?.recorders.remove(&session);
        Ok(())
    }

    /// Downmix and resample a captured buffer, returning `f32` mono samples.
    ///
    /// The sibling of `PrepareCapture` for callers that need samples rather
    /// than a container — a live loop measuring energy per frame and
    /// accumulating an utterance, rather than one finishing a recording.
    ///
    /// Exists so this work can leave a host's audio callback: `cpal` delivers
    /// on a realtime thread where the correct amount of work is as little as
    /// possible, and a host that forwards raw interleaved samples to its own
    /// worker does strictly less there than one that downmixes in place.
    async fn prepare_frames(
        &self,
        samples: String,
        source_rate: u32,
        channels: u16,
    ) -> TinyBusResult<String> {
        let raw = f32_samples(&decode_audio(&samples)?)?;
        let mono = audio::to_mono(&raw, channels).map_err(|e| failed(&e))?;
        check_expansion(mono.len(), source_rate)?;
        let resampled =
            audio::resample(&mono, source_rate, audio::STT_SAMPLE_RATE).map_err(|e| failed(&e))?;
        Ok(encode_samples(&resampled))
    }

    /// Root-mean-square energy of each fixed-size frame in a buffer.
    ///
    /// Paired with `VadPush`: a host slices its resampled audio into frames and
    /// needs one number per frame. Returning them together keeps the framing
    /// rule — including what happens to a short trailing frame — in one place
    /// rather than reimplemented by every caller.
    ///
    /// A trailing partial frame is measured as-is rather than dropped or padded.
    /// Dropping it would lose the end of an utterance; padding with silence
    /// would dilute its energy and could turn speech into a below-threshold
    /// frame.
    async fn frame_energies(&self, samples: String, frame_len: u32) -> TinyBusResult<Vec<f32>> {
        if frame_len == 0 {
            return Err(tinybus::Error::failed(
                "frame_len must be greater than zero".to_string(),
            ));
        }
        let samples = f32_samples(&decode_audio(&samples)?)?;
        Ok(samples.chunks(frame_len as usize).map(audio::rms).collect())
    }

    /// Wrap base64 little-endian **`i16`** PCM samples in a WAV container.
    ///
    /// Separate from `EncodeWav` because a caller that already holds 16-bit PCM
    /// — a client streaming headerless PCM16LE, say — would otherwise have to
    /// widen to `f32` and let this widen back. That round trip is lossy by one
    /// LSB (`/32768` out, `*32767` back), which is inaudible but is a silent
    /// change to the caller's samples for no reason. This path is exact.
    async fn encode_wav_pcm16(
        &self,
        samples: String,
        sample_rate: u32,
        channels: u16,
    ) -> TinyBusResult<String> {
        let bytes = decode_audio(&samples)?;
        if !bytes.len().is_multiple_of(2) {
            return Err(tinybus::Error::failed(format!(
                "{} bytes is not a whole number of i16 samples",
                bytes.len()
            )));
        }
        let pcm: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&sample| i16::from_le_bytes(sample))
            .collect();
        let wav = audio::pcm16_to_wav(&pcm, sample_rate, channels).map_err(|e| failed(&e))?;
        if wav.len() > MAX_AUDIO_BYTES {
            return Err(tinybus::Error::failed(format!(
                "encoded WAV exceeds the {MAX_AUDIO_BYTES} byte limit"
            )));
        }
        Ok(BASE64.encode(wav))
    }

    /// Wrap base64 little-endian `f32` mono samples in a 16-bit PCM WAV file.
    ///
    /// Returns the base64 WAV bytes.
    async fn encode_wav(&self, samples: String, sample_rate: u32) -> TinyBusResult<String> {
        let samples = f32_samples(&decode_audio(&samples)?)?;
        let wav = audio::f32_mono_to_wav(&samples, sample_rate).map_err(|e| failed(&e))?;
        if wav.len() > MAX_AUDIO_BYTES {
            return Err(tinybus::Error::failed(format!(
                "encoded WAV exceeds the {MAX_AUDIO_BYTES} byte limit"
            )));
        }
        Ok(BASE64.encode(wav))
    }

    /// Downmix, resample, and silence-gate a captured buffer, returning WAV.
    ///
    /// This is the whole capture-side pipeline in one call, because a host that
    /// made three round trips would ship the same audio across the bus three
    /// times to do work that is a few microseconds of arithmetic.
    ///
    /// `gate_threshold` of zero disables the silence gate.
    async fn prepare_capture(
        &self,
        samples: String,
        source_rate: u32,
        channels: u16,
        gate_threshold: f32,
    ) -> TinyBusResult<String> {
        let raw = f32_samples(&decode_audio(&samples)?)?;
        let mono = audio::to_mono(&raw, channels).map_err(|e| failed(&e))?;
        check_expansion(mono.len(), source_rate)?;
        let resampled =
            audio::resample(&mono, source_rate, audio::STT_SAMPLE_RATE).map_err(|e| failed(&e))?;

        let gated = if gate_threshold > 0.0 {
            let config = SilenceGateConfig {
                threshold: gate_threshold,
                ..SilenceGateConfig::default()
            };
            let mut gate =
                audio::SilenceGate::new(config, audio::STT_SAMPLE_RATE).map_err(|e| failed(&e))?;
            // One block: the gate is a streaming filter, but a caller handing
            // over a finished recording has no blocks to give it.
            gate.push(&resampled)
        } else {
            resampled
        };

        let wav = audio::f32_mono_to_wav(&gated, audio::STT_SAMPLE_RATE).map_err(|e| failed(&e))?;
        if wav.len() > MAX_AUDIO_BYTES {
            return Err(tinybus::Error::failed(format!(
                "encoded WAV exceeds the {MAX_AUDIO_BYTES} byte limit"
            )));
        }
        Ok(BASE64.encode(wav))
    }
    /// Reserve bounded hotkey state before native listener startup.
    async fn hotkey_reserve(
        &self,
        request: HotkeyReserveRequest,
    ) -> TinyBusResult<HotkeyResult<tinyvoice_bus::HotkeyHandle>> {
        Ok(self.hotkeys.reserve(request))
    }
    /// Start a reserved listener; retries return the same running lease.
    async fn hotkey_start(
        &self,
        request: HotkeyHandleRequest,
    ) -> TinyBusResult<HotkeyResult<tinyvoice_bus::HotkeyReply>> {
        let hotkeys = self.hotkeys.clone();
        tokio::task::spawn_blocking(move || hotkeys.start(&request))
            .await
            .map_err(|_| tinybus::Error::failed("hotkey startup worker failed"))
    }
    /// Read the retained oldest activation batch until it is acknowledged.
    async fn hotkey_read(
        &self,
        request: HotkeyReadRequest,
    ) -> TinyBusResult<HotkeyResult<tinyvoice_bus::HotkeyBatch>> {
        Ok(self.hotkeys.read(&request))
    }
    /// Feed generic, bounded host-owned key facts.
    async fn hotkey_feed(
        &self,
        request: HotkeyFeedRequest,
    ) -> TinyBusResult<HotkeyResult<tinyvoice_bus::HotkeyReply>> {
        Ok(self.hotkeys.feed(&request))
    }
    /// Stop a listener after its owned cleanup completes.
    async fn hotkey_stop(
        &self,
        request: HotkeyHandleRequest,
    ) -> TinyBusResult<HotkeyResult<tinyvoice_bus::HotkeyReply>> {
        let hotkeys = self.hotkeys.clone();
        tokio::task::spawn_blocking(move || hotkeys.stop(&request))
            .await
            .map_err(|_| tinybus::Error::failed("hotkey cleanup worker failed"))
    }
    /// Close admission and stop every listener before unloading the module.
    async fn hotkey_shutdown(&self) -> TinyBusResult<HotkeyResult<tinyvoice_bus::HotkeyReply>> {
        let hotkeys = self.hotkeys.clone();
        tokio::task::spawn_blocking(move || hotkeys.shutdown())
            .await
            .map_err(|_| tinybus::Error::failed("hotkey shutdown worker failed"))
    }
}

/// Claim the bus name and serve the interface.
async fn setup(connection: Connection) -> TinyBusResult<()> {
    connection
        .serve_at(
            names::OBJECT_PATH.try_into()?,
            VoiceService {
                live: std::sync::Arc::new(live::Sessions::with_connection(&connection)),
                relay: std::sync::Arc::new(relay_runtime::Turns::with_connection(&connection)),
                ..VoiceService::default()
            },
        )
        .await?;
    connection.request_name(names::BUS_NAME).await?;
    Ok(())
}

tinybus_module::module_export_optional_static! {
    setup = setup,
    worker_threads = 1,
    provides = ["ai.tinyhumans.tinyvoice.Voice"],
    methods = [
        "Route",
        "ExtractCommand",
        "WakeWordPresent",
        "IsHallucinated",
        "GateRecording",
        "SequenceUtterance",
        "Segment",
        "VadOpen",
        "VadPush",
        "VadIsSpeaking",
        "VadReset",
        "VadClose",
        "SessionOpen",
        "SessionAdvance",
        "SessionStatus",
        "SessionClose",
        "PrepareFrames",
        "PrepareRelayTurn",
        "RelayConfigure",
        "RelayReserve",
        "RelayStart",
        "RelayRelease",
        "RelayShutdown",
        "PrepareLiveInput",
        "LiveReserve",
        "LiveBindTransport",
        "LiveStart",
        "LiveSend",
        "LiveRead",
        "LiveRelease",
        "LiveShutdown",
        "FrameEnergies",
        "CaptureStart",
        "CapturePoll",
        "CaptureStop",
        "CaptureShutdown",
        "EncodeWav",
        "EncodeWavPcm16",
        "PrepareCapture",
        "ListInputDevices",
        "RecordingStart",
        "ReserveCapture",
        "RecordingFinish",
        "RecordingSummary",
        "RecordingCancel",
        "ReadAudioOutput",
        "ReleaseAudioOutput",
        "HotkeyReserve",
        "HotkeyStart",
        "HotkeyRead",
        "HotkeyFeed",
        "HotkeyStop",
        "HotkeyShutdown",

    ],
    signals = [],
    requires = [],
    optional = [],
    lazy = false,
}

mod capture;
mod hotkey;
