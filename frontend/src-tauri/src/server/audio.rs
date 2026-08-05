//! Live browser audio ingest and file upload transcription.
//!
//! The browser captures and mixes (mic + tab audio via WebAudio), then streams
//! 16 kHz mono Int16 PCM over a WebSocket. The server segments it, runs the same
//! Whisper/Parakeet engines the desktop app uses, and pushes `transcript-update`
//! events back over SSE so the existing UI renders them unchanged.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Multipart, Path as UrlPath, State};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};

use super::{sanitize_path_component, ApiError, SharedState};
use crate::database::repositories::setting::SettingsRepository;
use crate::database::repositories::transcript::TranscriptsRepository;

/// What both engines expect, and what the browser worklet sends.
pub const SAMPLE_RATE: u32 = 16_000;

// ---------------------------------------------------------------------------
// Segmenter
// ---------------------------------------------------------------------------

/// Splits a continuous stream into utterances on trailing silence.
///
/// ponytail: RMS gate instead of `audio::vad::ContinuousVadProcessor`. The Silero
/// model gives better boundaries but has to stay resident across awaits; swap it in
/// here if transcription quality at segment edges becomes a problem.
pub struct Segmenter {
    buffer: Vec<f32>,
    silence_run: usize,
    consumed: usize,
    silence_threshold: f32,
    min_samples: usize,
    silence_samples: usize,
    max_samples: usize,
}

/// One utterance, with timings relative to the start of the stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    pub samples: Vec<f32>,
    pub start_seconds: f64,
    pub end_seconds: f64,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmenter {
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(SAMPLE_RATE as usize * 8),
            silence_run: 0,
            consumed: 0,
            silence_threshold: 0.006,
            min_samples: SAMPLE_RATE as usize,
            silence_samples: (SAMPLE_RATE as f32 * 0.9) as usize,
            max_samples: SAMPLE_RATE as usize * 20,
        }
    }

    /// Feed samples, get back any utterances that just closed.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Utterance> {
        let mut out = Vec::new();
        // ~30 ms frames, matching what the browser worklet ships.
        let frame = (SAMPLE_RATE / 33) as usize;
        for chunk in samples.chunks(frame.max(1)) {
            self.buffer.extend_from_slice(chunk);
            if rms(chunk) < self.silence_threshold {
                self.silence_run += chunk.len();
            } else {
                self.silence_run = 0;
            }

            let long_enough = self.buffer.len() >= self.min_samples;
            let quiet = self.silence_run >= self.silence_samples;
            let too_long = self.buffer.len() >= self.max_samples;

            if (long_enough && quiet) || too_long {
                if let Some(utterance) = self.take() {
                    out.push(utterance);
                }
            }
        }
        out
    }

    /// Close out whatever is left, e.g. when the recording stops.
    pub fn flush(&mut self) -> Option<Utterance> {
        if self.buffer.len() < (SAMPLE_RATE as f32 * 0.25) as usize {
            self.buffer.clear();
            self.silence_run = 0;
            return None;
        }
        self.take()
    }

    fn take(&mut self) -> Option<Utterance> {
        if self.buffer.is_empty() {
            return None;
        }
        let samples = std::mem::take(&mut self.buffer);
        let start = self.consumed;
        self.consumed += samples.len();
        self.silence_run = 0;
        // All-silence stretches are dropped rather than sent to the engine.
        if rms(&samples) < self.silence_threshold {
            return None;
        }
        Some(Utterance {
            start_seconds: start as f64 / SAMPLE_RATE as f64,
            end_seconds: self.consumed as f64 / SAMPLE_RATE as f64,
            samples,
        })
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// Little-endian Int16 PCM straight off the socket.
pub fn pcm16_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
        .collect()
}

// ---------------------------------------------------------------------------
// Transcription
// ---------------------------------------------------------------------------

/// Run one utterance through whichever engine the transcript settings select.
pub async fn transcribe(
    state: &SharedState,
    samples: Vec<f32>,
    language: Option<String>,
) -> Result<String, String> {
    let provider = SettingsRepository::get_transcript_config(state.pool())
        .await
        .ok()
        .flatten()
        .and_then(|s| serde_json::to_value(&s).ok())
        .and_then(|v| v.get("provider").and_then(|p| p.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| "parakeet".to_string());

    let _permit = state
        .transcribe_lock
        .acquire()
        .await
        .map_err(|e| format!("transcription queue closed: {}", e))?;

    if provider == "localWhisper" {
        if !state.whisper.is_model_loaded().await {
            let models = state.whisper.discover_models().await.map_err(|e| e.to_string())?;
            let first = models
                .iter()
                .find(|m| {
                    matches!(
                        m.status,
                        crate::whisper_engine::whisper_engine::ModelStatus::Available
                    )
                })
                .ok_or_else(|| {
                    "No Whisper models are available. Download one in Settings.".to_string()
                })?;
            state.whisper.load_model(&first.name).await.map_err(|e| e.to_string())?;
        }
        state.whisper.transcribe_audio(samples, language).await.map_err(|e| e.to_string())
    } else {
        if !state.parakeet.is_model_loaded().await {
            let models = state.parakeet.discover_models().await.map_err(|e| e.to_string())?;
            let first = models
                .iter()
                .find(|m| matches!(m.status, crate::parakeet_engine::ModelStatus::Available))
                .ok_or_else(|| {
                    "No Parakeet models are available. Download one in Settings.".to_string()
                })?;
            state.parakeet.load_model(&first.name).await.map_err(|e| e.to_string())?;
        }
        state.parakeet.transcribe_audio(samples).await.map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Live WebSocket session
// ---------------------------------------------------------------------------

pub struct LiveSession {
    pub meeting_name: String,
    pub sequence: AtomicU64,
    pub speech_announced: AtomicBool,
}

#[derive(Debug, serde::Deserialize)]
struct Control {
    #[serde(default)]
    r#type: String,
    #[serde(default, alias = "meetingName")]
    meeting_name: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

pub async fn live_ws(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
    UrlPath(session_id): UrlPath<String>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = run_session(socket, state.clone(), session_id.clone()).await {
            log::warn!("[server] live session {} ended with error: {}", session_id, e);
            state.emit(
                "transcription-error",
                json!({
                    "error": e,
                    "userMessage": "Live transcription stopped unexpectedly. Check the server logs.",
                    "actionable": false
                }),
            );
        }
    })
}

async fn run_session(
    mut socket: WebSocket,
    state: SharedState,
    session_id: String,
) -> Result<(), String> {
    let session = Arc::new(LiveSession {
        meeting_name: format!("Meeting {}", chrono::Local::now().format("%d_%m_%y_%H_%M_%S")),
        sequence: AtomicU64::new(0),
        speech_announced: AtomicBool::new(false),
    });
    state.live.insert(session_id.clone(), session.clone());
    state.emit(
        "recording-started",
        json!({
            "message": "Recording started",
            "devices": ["Browser Microphone", "Browser System Audio"],
            "workers": 1
        }),
    );

    let mut segmenter = Segmenter::new();
    let mut language: Option<String> = None;

    while let Some(incoming) = socket.recv().await {
        let message = incoming.map_err(|e| e.to_string())?;
        match message {
            Message::Binary(bytes) => {
                let samples = pcm16_to_f32(&bytes);
                for utterance in segmenter.push(&samples) {
                    emit_utterance(&state, &session, utterance, language.clone()).await;
                }
            }
            Message::Text(text) => {
                let control: Control = serde_json::from_str(&text).unwrap_or(Control {
                    r#type: String::new(),
                    meeting_name: None,
                    language: None,
                });
                match control.r#type.as_str() {
                    "start" => {
                        if let Some(lang) = control.language.filter(|l| !l.trim().is_empty()) {
                            language = Some(lang);
                        }
                        log::info!(
                            "[server] live session {} started ({})",
                            session_id,
                            control.meeting_name.as_deref().unwrap_or(&session.meeting_name)
                        );
                    }
                    "stop" => break,
                    other => log::debug!("[server] ignoring control frame `{}`", other),
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    if let Some(utterance) = segmenter.flush() {
        emit_utterance(&state, &session, utterance, language.clone()).await;
    }

    state.live.remove(&session_id);
    // The frontend owns persistence: it calls api_save_transcript once it has every
    // segment, exactly as it does on the desktop.
    state.emit("transcription-complete", Value::Null);
    state.emit(
        "recording-stopped",
        json!({ "message": "Recording stopped", "meeting_name": session.meeting_name }),
    );
    Ok(())
}

async fn emit_utterance(
    state: &SharedState,
    session: &Arc<LiveSession>,
    utterance: Utterance,
    language: Option<String>,
) {
    let start = utterance.start_seconds;
    let end = utterance.end_seconds;
    match transcribe(state, utterance.samples, language).await {
        Ok(text) if !text.trim().is_empty() => {
            if !session.speech_announced.swap(true, Ordering::Relaxed) {
                state.emit("speech-detected", json!({ "message": "Speech activity detected" }));
            }
            let sequence = session.sequence.fetch_add(1, Ordering::Relaxed);
            state.emit(
                "transcript-update",
                json!({
                    "text": text.trim(),
                    "timestamp": chrono::Local::now().format("%H:%M:%S").to_string(),
                    "source": "Audio",
                    "sequence_id": sequence,
                    "chunk_start_time": start,
                    "is_partial": false,
                    "confidence": 0.85,
                    "audio_start_time": start,
                    "audio_end_time": end,
                    "duration": end - start,
                }),
            );
        }
        Ok(_) => {}
        Err(e) => {
            log::warn!("[server] transcription failed: {}", e);
            state.emit(
                "transcription-error",
                json!({ "error": e, "userMessage": e, "actionable": false }),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// File upload
// ---------------------------------------------------------------------------

/// `POST /api/upload` — multipart `file` plus optional `meeting_name`.
/// Decoding and transcription run in the background; progress arrives over SSE and
/// the finished meeting is saved to the database.
pub async fn upload(
    State(state): State<SharedState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    let mut meeting_name: Option<String> = None;
    let mut saved: Option<PathBuf> = None;

    let upload_dir = state.data_dir.join("uploads");
    tokio::fs::create_dir_all(&upload_dir).await?;

    while let Some(part) =
        multipart.next_field().await.map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        match part.name().unwrap_or_default() {
            "meeting_name" => {
                meeting_name = part.text().await.ok().filter(|t| !t.trim().is_empty());
            }
            "file" => {
                let filename = part.file_name().unwrap_or("upload").to_string();
                let extension = std::path::Path::new(&filename)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| sanitize_path_component(e))
                    .unwrap_or_else(|| "wav".to_string());
                let bytes =
                    part.bytes().await.map_err(|e| ApiError::bad_request(e.to_string()))?;
                if bytes.is_empty() {
                    return Err(ApiError::bad_request("Uploaded file is empty"));
                }
                let target = upload_dir.join(format!("{}.{}", uuid::Uuid::new_v4(), extension));
                tokio::fs::write(&target, &bytes).await?;
                saved = Some(target);
            }
            other => log::debug!("[server] ignoring upload field `{}`", other),
        }
    }

    let Some(path) = saved else {
        return Err(ApiError::bad_request("Missing `file` field"));
    };
    let meeting_name = meeting_name
        .unwrap_or_else(|| format!("Import {}", chrono::Local::now().format("%d_%m_%y_%H_%M_%S")));

    let job_state = state.clone();
    let job_name = meeting_name.clone();
    tokio::spawn(async move {
        if let Err(e) = transcribe_file(job_state.clone(), path.clone(), job_name).await {
            log::error!("[server] upload transcription failed: {}", e);
            job_state.emit(
                "transcription-error",
                json!({ "error": e, "userMessage": e, "actionable": false }),
            );
        }
        let _ = tokio::fs::remove_file(&path).await;
    });

    Ok(Json(json!({ "status": "processing", "meeting_name": meeting_name })))
}

async fn transcribe_file(
    state: SharedState,
    path: PathBuf,
    meeting_name: String,
) -> Result<(), String> {
    let decode_path = path.clone();
    // Decoding and VAD are CPU-bound and fully synchronous.
    let chunks =
        tokio::task::spawn_blocking(move || -> Result<Vec<(Vec<f32>, f64, f64)>, String> {
            let decoded = crate::audio::decoder::decode_audio_file(&decode_path)
                .map_err(|e| e.to_string())?;
            let samples = decoded.to_whisper_format();
            let segments =
                crate::audio::vad::get_speech_chunks(&samples, 2000).map_err(|e| e.to_string())?;
            Ok(segments
                .into_iter()
                .map(|s| (s.samples, s.start_timestamp_ms / 1000.0, s.end_timestamp_ms / 1000.0))
                .collect())
        })
        .await
        .map_err(|e| e.to_string())??;

    let total = chunks.len();
    if total == 0 {
        return Err("No speech found in the uploaded audio".to_string());
    }

    let mut segments: Vec<Value> = Vec::with_capacity(total);
    for (index, (samples, start, end)) in chunks.into_iter().enumerate() {
        let text = transcribe(&state, samples, None).await?;
        if !text.trim().is_empty() {
            segments.push(json!({
                "id": format!("seg_{}", index),
                "text": text.trim(),
                "timestamp": format_clock(start),
                "audio_start_time": start,
                "audio_end_time": end,
                "duration": end - start,
            }));
        }
        state.emit(
            "upload-progress",
            json!({
                "meeting_name": meeting_name,
                "progress": ((index + 1) * 100 / total) as u8,
            }),
        );
    }

    let parsed: Vec<crate::api::TranscriptSegment> =
        serde_json::from_value(Value::Array(segments.clone())).map_err(|e| e.to_string())?;
    let meeting_id =
        TranscriptsRepository::save_transcript(state.pool(), &meeting_name, &parsed, None)
            .await
            .map_err(|e| e.to_string())?;

    state.emit(
        "upload-complete",
        json!({
            "meeting_id": meeting_id,
            "meeting_name": meeting_name,
            "segments": segments.len()
        }),
    );
    Ok(())
}

fn format_clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", total / 3600, (total % 3600) / 60, total % 60)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds: f32) -> Vec<f32> {
        let n = (SAMPLE_RATE as f32 * seconds) as usize;
        (0..n)
            .map(|i| {
                (i as f32 * 440.0 * 2.0 * std::f32::consts::PI / SAMPLE_RATE as f32).sin() * 0.3
            })
            .collect()
    }

    fn silence(seconds: f32) -> Vec<f32> {
        vec![0.0; (SAMPLE_RATE as f32 * seconds) as usize]
    }

    #[test]
    fn splits_on_trailing_silence() {
        let mut segmenter = Segmenter::new();
        let mut input = tone(1.5);
        input.extend(silence(1.2));
        let out = segmenter.push(&input);
        assert_eq!(out.len(), 1, "one utterance should close on the silence");
        assert!(out[0].start_seconds < 0.01);
        assert!(out[0].end_seconds > 1.4, "end was {}", out[0].end_seconds);

        // A second utterance is timed against the whole stream, not the buffer.
        let mut next = tone(1.5);
        next.extend(silence(1.2));
        let out2 = segmenter.push(&next);
        assert_eq!(out2.len(), 1);
        assert!(out2[0].start_seconds > 2.0, "start was {}", out2[0].start_seconds);
    }

    #[test]
    fn drops_pure_silence_and_caps_long_speech() {
        let mut segmenter = Segmenter::new();
        assert!(segmenter.push(&silence(5.0)).is_empty(), "silence must not reach the engine");

        let mut long = Segmenter::new();
        let out = long.push(&tone(25.0));
        assert_eq!(out.len(), 1, "the 20s cap should force one cut");
        assert!(out[0].end_seconds <= 20.5);
    }

    #[test]
    fn decodes_pcm16_frames() {
        let bytes = [0x00, 0x00, 0x00, 0x40, 0x00, 0xC0];
        let samples = pcm16_to_f32(&bytes);
        assert_eq!(samples.len(), 3);
        assert!(samples[0].abs() < f32::EPSILON);
        assert!((samples[1] - 0.5).abs() < 0.001, "got {}", samples[1]);
        assert!((samples[2] + 0.5).abs() < 0.001, "got {}", samples[2]);
    }
}
