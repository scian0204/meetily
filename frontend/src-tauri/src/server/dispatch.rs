//! `invoke()` command dispatch for the web build.
//!
//! Each arm reimplements one `#[tauri::command]` against the same repositories and
//! engines, minus `AppHandle`/`State`. Wire shapes (camelCase arg keys, response
//! field names) match the desktop app so the frontend needs no changes.
//!
//! Commands that only make sense on a desktop (tray, native folders, permissions,
//! analytics, onboarding, recording control) are handled in the browser shim and
//! never reach here. Anything unknown returns 404 and is logged — never silently
//! swallowed.

use std::time::Instant;

use serde_json::{json, Value};

use super::{ApiError, SharedState};
use crate::database::repositories::meeting::MeetingsRepository;
use crate::database::repositories::setting::SettingsRepository;
use crate::database::repositories::summary::SummaryProcessesRepository;
use crate::database::repositories::transcript::TranscriptsRepository;
use crate::database::repositories::transcript_chunk::TranscriptChunksRepository;
use crate::parakeet_engine::ModelStatus as ParakeetStatus;
use crate::summary::{templates, CustomOpenAIConfig, LLMProvider};
use crate::whisper_engine::whisper_engine::ModelStatus as WhisperStatus;

// ---------------------------------------------------------------------------
// Arg helpers
// ---------------------------------------------------------------------------

fn field(args: &Value, key: &str) -> Value {
    args.get(key).cloned().unwrap_or(Value::Null)
}

fn req_str(args: &Value, key: &str) -> Result<String, ApiError> {
    match args.get(key).and_then(|v| v.as_str()) {
        Some(s) => Ok(s.to_string()),
        None => Err(ApiError::bad_request(format!("missing required argument `{}`", key))),
    }
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn req_i64(args: &Value, key: &str) -> Result<i64, ApiError> {
    match args.get(key).and_then(|v| v.as_i64()) {
        Some(v) => Ok(v),
        None => Err(ApiError::bad_request(format!("missing required argument `{}`", key))),
    }
}

fn opt_i32(args: &Value, key: &str) -> Option<i32> {
    args.get(key).and_then(|v| v.as_i64()).map(|v| v as i32)
}

fn opt_f32(args: &Value, key: &str) -> Option<f32> {
    args.get(key).and_then(|v| v.as_f64()).map(|v| v as f32)
}

/// Pick a key out of a serialized struct without needing to know the rest of it.
fn pick(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, ApiError> {
    serde_json::to_value(v).map_err(|e| ApiError::internal(e.to_string()))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub async fn dispatch(state: &SharedState, cmd: &str, args: Value) -> Result<Value, ApiError> {
    let pool = state.pool();

    match cmd {
        // ---------------- meetings / transcripts ----------------
        "api_get_meetings" => {
            let meetings = MeetingsRepository::get_meetings(pool).await?;
            let list: Vec<Value> = meetings
                .iter()
                .map(|m| {
                    let v = serde_json::to_value(m).unwrap_or(Value::Null);
                    json!({ "id": pick(&v, "id"), "title": pick(&v, "title") })
                })
                .collect();
            Ok(Value::Array(list))
        }

        "api_get_meeting" => {
            let id = req_str(&args, "meetingId")?;
            match MeetingsRepository::get_meeting(pool, &id).await? {
                Some(details) => to_value(&details),
                None => Err(ApiError::not_found(format!("Meeting not found: {}", id))),
            }
        }

        "api_get_meeting_metadata" => {
            let id = req_str(&args, "meetingId")?;
            match MeetingsRepository::get_meeting_metadata(pool, &id).await? {
                Some(model) => to_value(&model),
                None => Err(ApiError::not_found(format!("Meeting not found: {}", id))),
            }
        }

        "api_get_meeting_transcripts" => {
            let id = req_str(&args, "meetingId")?;
            let limit = req_i64(&args, "limit")?;
            let offset = req_i64(&args, "offset")?;
            let (rows, total) =
                MeetingsRepository::get_meeting_transcripts_paginated(pool, &id, limit, offset)
                    .await?;
            let transcripts: Vec<Value> = rows.iter().map(transcript_to_wire).collect();
            let has_more = (offset + transcripts.len() as i64) < total;
            Ok(json!({ "transcripts": transcripts, "total_count": total, "has_more": has_more }))
        }

        "api_save_transcript" => {
            let title = req_str(&args, "meetingTitle")?;
            let folder_path = opt_str(&args, "folderPath");
            let segments: Vec<crate::api::TranscriptSegment> =
                serde_json::from_value(field(&args, "transcripts")).map_err(|e| {
                    ApiError::bad_request(format!(
                        "Invalid transcript data format: {}. Please check the data structure.",
                        e
                    ))
                })?;
            let meeting_id =
                TranscriptsRepository::save_transcript(pool, &title, &segments, folder_path).await?;
            Ok(json!({
                "status": "success",
                "message": "Transcript saved successfully",
                "meeting_id": meeting_id
            }))
        }

        "api_delete_meeting" => {
            let id = req_str(&args, "meetingId")?;
            if MeetingsRepository::delete_meeting(pool, &id).await? {
                Ok(json!({ "status": "success", "message": "Meeting deleted successfully" }))
            } else {
                Err(ApiError::not_found(format!(
                    "Meeting not found or could not be deleted: {}",
                    id
                )))
            }
        }

        "api_save_meeting_title" => {
            let id = req_str(&args, "meetingId")?;
            let title = req_str(&args, "title")?;
            if MeetingsRepository::update_meeting_title(pool, &id, &title).await? {
                Ok(json!({ "message": "Meeting title saved successfully" }))
            } else {
                Err(ApiError::not_found(format!("No meeting found with id {}", id)))
            }
        }

        "api_search_transcripts" => {
            let query = req_str(&args, "query")?;
            let results = TranscriptsRepository::search_transcripts(pool, &query).await?;
            to_value(&results)
        }

        // ---------------- settings ----------------
        "api_get_model_config" => {
            let Some(setting) = SettingsRepository::get_model_config(pool).await? else {
                return Ok(Value::Null);
            };
            let raw = to_value(&setting)?;
            let provider = pick(&raw, "provider").as_str().unwrap_or_default().to_string();
            let api_key = SettingsRepository::get_api_key(pool, &provider).await.unwrap_or(None);
            let endpoint = match state.config.ollama_endpoint.clone() {
                Some(e) => Value::String(e),
                None => pick(&raw, "ollamaEndpoint"),
            };
            Ok(json!({
                "provider": provider,
                "model": pick(&raw, "model"),
                "whisperModel": pick(&raw, "whisperModel"),
                "apiKey": api_key,
                "ollamaEndpoint": endpoint,
            }))
        }

        "api_save_model_config" => {
            let provider = req_str(&args, "provider")?;
            let model = req_str(&args, "model")?;
            let whisper_model = req_str(&args, "whisperModel")?;
            let ollama_endpoint = opt_str(&args, "ollamaEndpoint");
            SettingsRepository::save_model_config(
                pool,
                &provider,
                &model,
                &whisper_model,
                ollama_endpoint.as_deref(),
            )
            .await?;
            if let Some(key) = opt_str(&args, "apiKey").filter(|k| !k.trim().is_empty()) {
                if provider != "custom-openai" {
                    SettingsRepository::save_api_key(pool, &provider, &key).await?;
                }
            }
            Ok(json!({ "status": "success", "message": "Model configuration saved successfully" }))
        }

        "api_get_transcript_config" => match SettingsRepository::get_transcript_config(pool).await? {
            Some(setting) => {
                let raw = to_value(&setting)?;
                let provider = pick(&raw, "provider").as_str().unwrap_or_default().to_string();
                let api_key = SettingsRepository::get_transcript_api_key(pool, &provider)
                    .await
                    .unwrap_or(None);
                Ok(json!({
                    "provider": provider,
                    "model": pick(&raw, "model"),
                    "apiKey": api_key,
                }))
            }
            None => Ok(json!({
                "provider": "parakeet",
                "model": crate::config::DEFAULT_PARAKEET_MODEL,
                "apiKey": Value::Null,
            })),
        },

        "api_save_transcript_config" => {
            let provider = req_str(&args, "provider")?;
            let model = req_str(&args, "model")?;
            SettingsRepository::save_transcript_config(pool, &provider, &model).await?;
            if let Some(key) = opt_str(&args, "apiKey").filter(|k| !k.trim().is_empty()) {
                SettingsRepository::save_transcript_api_key(pool, &provider, &key).await?;
            }
            Ok(json!({
                "status": "success",
                "message": "Transcript configuration saved successfully"
            }))
        }

        "api_get_api_key" => {
            let provider = req_str(&args, "provider")?;
            let key = SettingsRepository::get_api_key(pool, &provider).await?;
            Ok(Value::String(key.unwrap_or_default()))
        }

        "api_get_transcript_api_key" => {
            let provider = req_str(&args, "provider")?;
            let key = SettingsRepository::get_transcript_api_key(pool, &provider).await?;
            Ok(Value::String(key.unwrap_or_default()))
        }

        "api_get_custom_openai_config" => {
            match SettingsRepository::get_custom_openai_config(pool).await? {
                Some(config) => to_value(&config),
                None => Ok(Value::Null),
            }
        }

        "api_save_custom_openai_config" => {
            let endpoint = req_str(&args, "endpoint")?;
            let model = req_str(&args, "model")?;
            let temperature = opt_f32(&args, "temperature");
            let top_p = opt_f32(&args, "topP");
            let max_tokens = opt_i32(&args, "maxTokens");

            if endpoint.trim().is_empty() {
                return Err(ApiError::bad_request("Endpoint URL is required"));
            }
            if model.trim().is_empty() {
                return Err(ApiError::bad_request("Model name is required"));
            }
            if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
                return Err(ApiError::bad_request("Endpoint must start with http:// or https://"));
            }
            if let Some(t) = temperature {
                if !(0.0..=2.0).contains(&t) {
                    return Err(ApiError::bad_request("Temperature must be between 0.0 and 2.0"));
                }
            }
            if let Some(p) = top_p {
                if !(0.0..=1.0).contains(&p) {
                    return Err(ApiError::bad_request("Top P must be between 0.0 and 1.0"));
                }
            }
            if let Some(m) = max_tokens {
                if m < 1 {
                    return Err(ApiError::bad_request("Max tokens must be at least 1"));
                }
            }

            let config = CustomOpenAIConfig {
                endpoint: endpoint.trim().to_string(),
                api_key: opt_str(&args, "apiKey").filter(|k| !k.trim().is_empty()),
                model: model.trim().to_string(),
                max_tokens,
                temperature,
                top_p,
            };
            SettingsRepository::save_custom_openai_config(pool, &config).await?;
            Ok(json!({
                "status": "success",
                "message": "Custom OpenAI configuration saved successfully"
            }))
        }

        "api_test_custom_openai_connection" => {
            let endpoint = req_str(&args, "endpoint")?;
            let model = req_str(&args, "model")?;
            if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
                return Err(ApiError::bad_request("Endpoint must start with http:// or https://"));
            }
            let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
            let mut request = state.http.post(&url).json(&json!({
                "model": model,
                "messages": [{ "role": "user", "content": "Hi" }],
                "max_tokens": 5
            }));
            if let Some(key) = opt_str(&args, "apiKey").filter(|k| !k.trim().is_empty()) {
                request = request.bearer_auth(key);
            }
            let response = request.send().await.map_err(|e| {
                if e.is_timeout() {
                    ApiError::bad_request("Connection timed out. Please check the endpoint URL.")
                } else if e.is_connect() {
                    ApiError::bad_request(
                        "Could not connect to endpoint. Please verify the URL is correct and the server is running.",
                    )
                } else {
                    ApiError::bad_request(format!("Connection failed: {}", e))
                }
            })?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(ApiError::bad_request(format!(
                    "Connection failed with status {}: {}",
                    status.as_u16(),
                    body
                )));
            }
            let parsed: Value = serde_json::from_str(&body).map_err(|e| {
                ApiError::bad_request(format!(
                    "Endpoint is reachable but returned invalid JSON: {}. Response: {}",
                    e, body
                ))
            })?;
            let usable = parsed
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .map(|m| m.get("content").is_some() || m.get("reasoning_content").is_some())
                .unwrap_or(false);
            if !usable {
                return Err(ApiError::bad_request(
                    "Endpoint is reachable but doesn't appear to be OpenAI-compatible. Response is missing 'choices' array or 'message.content' / 'message.reasoning_content' field.",
                ));
            }
            Ok(json!({
                "status": "success",
                "message": "Connection successful and response validated",
                "http_status": status.as_u16()
            }))
        }

        // ---------------- summary ----------------
        "api_get_summary" => {
            let meeting_id = req_str(&args, "meetingId")?;
            let title = match MeetingsRepository::get_meeting_metadata(pool, &meeting_id).await {
                Ok(Some(model)) => pick(&to_value(&model)?, "title"),
                _ => Value::Null,
            };

            match SummaryProcessesRepository::get_summary_data_for_meeting(pool, &meeting_id).await?
            {
                Some(process) => {
                    let raw = to_value(&process)?;
                    let status =
                        pick(&raw, "status").as_str().unwrap_or("idle").to_lowercase();
                    let data = pick(&raw, "result")
                        .as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .unwrap_or(Value::Null);
                    Ok(json!({
                        "status": status,
                        "meetingName": title,
                        "meeting_id": meeting_id,
                        "start": pick(&raw, "start_time"),
                        "end": pick(&raw, "end_time"),
                        "data": data,
                        "error": pick(&raw, "error"),
                    }))
                }
                None => Ok(json!({
                    "status": "idle",
                    "meetingName": title,
                    "meeting_id": meeting_id,
                    "start": Value::Null,
                    "end": Value::Null,
                    "data": Value::Null,
                    "error": Value::Null,
                })),
            }
        }

        "api_process_transcript" => {
            let text = req_str(&args, "text")?;
            let provider_id = req_str(&args, "model")?;
            let model_name = req_str(&args, "modelName")?;
            let meeting_id = opt_str(&args, "meetingId")
                .unwrap_or_else(|| format!("meeting-{}", uuid::Uuid::new_v4()));
            let chunk_size = opt_i32(&args, "chunkSize").unwrap_or(40_000);
            let overlap = opt_i32(&args, "overlap").unwrap_or(1_000);
            let custom_prompt = opt_str(&args, "customPrompt").unwrap_or_default();
            let template_id =
                opt_str(&args, "templateId").unwrap_or_else(|| "daily_standup".to_string());
            let summary_language =
                opt_str(&args, "summaryLanguage").filter(|l| !l.trim().is_empty());

            SummaryProcessesRepository::create_or_reset_process(pool, &meeting_id).await?;
            TranscriptChunksRepository::save_transcript_data(
                pool,
                &meeting_id,
                &text,
                &provider_id,
                &model_name,
                chunk_size,
                overlap,
            )
            .await?;

            let job = SummaryJob {
                state: state.clone(),
                meeting_id: meeting_id.clone(),
                text,
                provider_id,
                model_name,
                custom_prompt,
                template_id,
                summary_language,
            };
            tokio::spawn(async move { job.run().await });

            Ok(json!({ "message": "Summary generation started", "process_id": meeting_id }))
        }

        // ponytail: cancellation is DB-flagged only; the in-flight LLM request finishes and
        // its result is discarded. Wire a CancellationToken registry if wasted tokens matter.
        "api_cancel_summary" => {
            let meeting_id = req_str(&args, "meetingId")?;
            SummaryProcessesRepository::update_process_cancelled(pool, &meeting_id).await?;
            Ok(json!({
                "message": "Summary generation cancelled successfully",
                "meeting_id": meeting_id
            }))
        }

        "api_save_meeting_summary" => {
            let meeting_id = req_str(&args, "meetingId")?;
            let summary = field(&args, "summary");
            if summary.is_null() {
                return Err(ApiError::bad_request("missing required argument `summary`"));
            }
            if SummaryProcessesRepository::update_meeting_summary(pool, &meeting_id, &summary)
                .await?
            {
                Ok(json!({ "message": "Meeting summary saved successfully" }))
            } else {
                Err(ApiError::not_found("Meeting not found or can't convert the json"))
            }
        }

        "api_list_templates" => {
            let list: Vec<Value> = templates::list_templates()
                .into_iter()
                .map(|(id, name, description)| {
                    json!({ "id": id, "name": name, "description": description })
                })
                .collect();
            Ok(Value::Array(list))
        }

        // Per-meeting language pins live in the meeting folder on desktop. On the server we
        // report `local_fallback` so the frontend keeps using its own localStorage copy.
        "api_get_meeting_summary_language" | "api_save_meeting_summary_language" => {
            Ok(json!({ "language": Value::Null, "storage": "local_fallback" }))
        }

        // Frontend-only flag on desktop too (the command does not exist there).
        "api_get_auto_generate_setting" => Ok(Value::Bool(true)),

        // ---------------- LLM provider catalogs ----------------
        "get_ollama_models" => {
            let endpoint =
                opt_str(&args, "endpoint").or_else(|| state.config.ollama_endpoint.clone());
            let models = crate::ollama::ollama::get_ollama_models(endpoint).await?;
            to_value(&models)
        }

        "delete_ollama_model" => {
            let model_name = req_str(&args, "modelName")?;
            let endpoint =
                opt_str(&args, "endpoint").or_else(|| state.config.ollama_endpoint.clone());
            crate::ollama::ollama::delete_ollama_model(model_name, endpoint).await?;
            Ok(Value::Null)
        }

        "get_openai_models" => {
            let models = crate::openai::openai::get_openai_models(opt_str(&args, "apiKey")).await?;
            to_value(&models)
        }

        "get_anthropic_models" => {
            let models =
                crate::anthropic::anthropic::get_anthropic_models(opt_str(&args, "apiKey")).await?;
            to_value(&models)
        }

        "get_groq_models" => {
            let models = crate::groq::groq::get_groq_models(opt_str(&args, "apiKey")).await?;
            to_value(&models)
        }

        // Blocking reqwest upstream, so it must not run on an async worker thread.
        "get_openrouter_models" => {
            let models =
                tokio::task::spawn_blocking(crate::openrouter::openrouter::get_openrouter_models)
                    .await
                    .map_err(|e| ApiError::internal(e.to_string()))??;
            to_value(&models)
        }

        // ---------------- whisper ----------------
        "whisper_init" => Ok(Value::Null),

        "whisper_get_available_models" => {
            let models = state.whisper.discover_models().await?;
            to_value(&models)
        }

        "whisper_has_available_models" => {
            let models = state.whisper.discover_models().await.unwrap_or_default();
            Ok(Value::Bool(models.iter().any(|m| matches!(m.status, WhisperStatus::Available))))
        }

        "whisper_load_model" => {
            let name = req_str(&args, "modelName")?;
            state.emit("model-loading-started", json!({ "modelName": name }));
            match state.whisper.load_model(&name).await {
                Ok(()) => {
                    state.emit("model-loading-completed", json!({ "modelName": name }));
                    Ok(Value::Null)
                }
                Err(e) => {
                    state.emit(
                        "model-loading-failed",
                        json!({ "modelName": name, "error": e.to_string() }),
                    );
                    Err(e.into())
                }
            }
        }

        "whisper_is_model_loaded" => Ok(Value::Bool(state.whisper.is_model_loaded().await)),

        "whisper_get_current_model" => match state.whisper.get_current_model().await {
            Some(name) => Ok(Value::String(name)),
            None => Ok(Value::Null),
        },

        "whisper_get_models_directory" => Ok(Value::String(
            state.whisper.get_models_directory().await.to_string_lossy().to_string(),
        )),

        "whisper_validate_model_ready" => {
            if let Some(current) = state.whisper.get_current_model().await {
                return Ok(Value::String(current));
            }
            let models = state.whisper.discover_models().await?;
            let first = models
                .iter()
                .find(|m| matches!(m.status, WhisperStatus::Available))
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "No Whisper models are available. Please download a model to enable transcription.",
                    )
                })?;
            state.whisper.load_model(&first.name).await?;
            Ok(Value::String(first.name.clone()))
        }

        "whisper_download_model" => {
            let name = req_str(&args, "modelName")?;
            download_whisper(state.clone(), name);
            Ok(Value::Null)
        }

        "whisper_cancel_download" => {
            let name = req_str(&args, "modelName")?;
            state.whisper.cancel_download(&name).await?;
            Ok(Value::Null)
        }

        "whisper_delete_corrupted_model" => {
            let name = req_str(&args, "modelName")?;
            Ok(Value::String(state.whisper.delete_model(&name).await?))
        }

        // ---------------- parakeet ----------------
        "parakeet_init" => Ok(Value::Null),

        "parakeet_get_available_models" => {
            let models = state.parakeet.discover_models().await?;
            to_value(&models)
        }

        "parakeet_has_available_models" => {
            let models = state.parakeet.discover_models().await.unwrap_or_default();
            Ok(Value::Bool(models.iter().any(|m| matches!(m.status, ParakeetStatus::Available))))
        }

        "parakeet_load_model" => {
            let name = req_str(&args, "modelName")?;
            state.emit("parakeet-model-loading-started", json!({ "modelName": name }));
            match state.parakeet.load_model(&name).await {
                Ok(()) => {
                    state.emit("parakeet-model-loading-completed", json!({ "modelName": name }));
                    Ok(Value::Null)
                }
                Err(e) => {
                    state.emit(
                        "parakeet-model-loading-failed",
                        json!({ "modelName": name, "error": e.to_string() }),
                    );
                    Err(e.into())
                }
            }
        }

        "parakeet_is_model_loaded" => Ok(Value::Bool(state.parakeet.is_model_loaded().await)),

        "parakeet_get_current_model" => match state.parakeet.get_current_model().await {
            Some(name) => Ok(Value::String(name)),
            None => Ok(Value::Null),
        },

        "parakeet_get_models_directory" => Ok(Value::String(
            state.parakeet.get_models_directory().await.to_string_lossy().to_string(),
        )),

        "parakeet_validate_model_ready" => {
            if let Some(current) = state.parakeet.get_current_model().await {
                return Ok(Value::String(current));
            }
            let models = state.parakeet.discover_models().await?;
            let available: Vec<_> =
                models.iter().filter(|m| matches!(m.status, ParakeetStatus::Available)).collect();
            let chosen = available
                .iter()
                .find(|m| format!("{:?}", m.quantization) == "Int8")
                .or_else(|| available.first())
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "No Parakeet models are available. Please download a model to enable fast transcription.",
                    )
                })?;
            state.parakeet.load_model(&chosen.name).await?;
            Ok(Value::String(chosen.name.clone()))
        }

        "parakeet_download_model" | "parakeet_retry_download" => {
            let name = req_str(&args, "modelName")?;
            download_parakeet(state.clone(), name);
            Ok(Value::Null)
        }

        "parakeet_cancel_download" => {
            let name = req_str(&args, "modelName")?;
            state.parakeet.cancel_download(&name).await?;
            state.emit(
                "parakeet-model-download-progress",
                json!({ "modelName": name, "progress": 0, "status": "cancelled" }),
            );
            Ok(Value::Null)
        }

        "parakeet_delete_corrupted_model" => {
            let name = req_str(&args, "modelName")?;
            Ok(Value::String(state.parakeet.delete_model(&name).await?))
        }

        // ---------------- paths / misc ----------------
        "get_database_directory" => Ok(Value::String(state.data_dir.to_string_lossy().to_string())),

        "get_default_recordings_folder_path" => Ok(Value::String(
            state.data_dir.join("recordings").to_string_lossy().to_string(),
        )),

        "get_recording_preferences" => Ok(json!({
            "save_folder": state.data_dir.join("recordings").to_string_lossy().to_string(),
            "auto_save": true,
            "file_format": "mp4",
            "preferred_mic_device": Value::Null,
            "preferred_system_device": Value::Null,
        })),

        // Browser-side concerns; accepted so the settings UI does not error.
        "set_recording_preferences" | "set_notification_settings" | "set_language_preference" => {
            Ok(Value::Null)
        }

        "get_notification_settings" => Ok(Value::Null),

        // The server always has a database; legacy import is desktop-only.
        "check_first_launch" => Ok(Value::Bool(false)),
        "check_default_legacy_database" | "detect_legacy_database" | "check_homebrew_database" => {
            Ok(Value::Null)
        }

        "initialize_fresh_database" => {
            let _ = SettingsRepository::save_transcript_config(
                pool,
                "parakeet",
                crate::config::DEFAULT_PARAKEET_MODEL,
            )
            .await;
            state.emit("database-initialized", Value::Null);
            Ok(Value::Null)
        }

        // The bundled llama.cpp sidecar is not shipped in the server image; report it as
        // unavailable so the UI offers Ollama / cloud providers instead.
        "builtin_ai_list_models" => Ok(Value::Array(vec![])),
        "builtin_ai_is_model_ready" => Ok(Value::Bool(false)),
        "builtin_ai_get_model_info" | "builtin_ai_get_available_summary_model" => Ok(Value::Null),

        unknown => {
            log::warn!("[server] unsupported command: {}", unknown);
            Err(ApiError::not_found(format!(
                "Command `{}` is not available in the self-hosted web build",
                unknown
            )))
        }
    }
}

/// `transcripts` rows expose `transcript`; the frontend expects `text`.
fn transcript_to_wire<T: serde::Serialize>(row: &T) -> Value {
    let raw = serde_json::to_value(row).unwrap_or(Value::Null);
    let mut out = serde_json::Map::new();
    out.insert("id".into(), pick(&raw, "id"));
    out.insert("text".into(), pick(&raw, "transcript"));
    out.insert("timestamp".into(), pick(&raw, "timestamp"));
    for key in ["audio_start_time", "audio_end_time", "duration"] {
        let value = pick(&raw, key);
        if !value.is_null() {
            out.insert(key.into(), value);
        }
    }
    Value::Object(out)
}

fn download_whisper(state: SharedState, name: String) {
    tokio::spawn(async move {
        let progress_state = state.clone();
        let progress_name = name.clone();
        let callback = Box::new(move |percent: u8| {
            progress_state.emit(
                "model-download-progress",
                json!({ "modelName": progress_name, "progress": percent }),
            );
        });
        match state.whisper.download_model(&name, Some(callback)).await {
            Ok(()) => state.emit("model-download-complete", json!({ "modelName": name })),
            Err(e) => state.emit(
                "model-download-error",
                json!({ "modelName": name, "error": e.to_string() }),
            ),
        }
    });
}

fn download_parakeet(state: SharedState, name: String) {
    tokio::spawn(async move {
        let progress_state = state.clone();
        let progress_name = name.clone();
        let callback = Box::new(move |percent: u8| {
            progress_state.emit(
                "parakeet-model-download-progress",
                json!({
                    "modelName": progress_name,
                    "progress": percent,
                    "status": if percent >= 100 { "completed" } else { "downloading" }
                }),
            );
        });
        match state.parakeet.download_model(&name, Some(callback)).await {
            Ok(()) => state.emit("parakeet-model-download-complete", json!({ "modelName": name })),
            Err(e) => state.emit(
                "parakeet-model-download-error",
                json!({ "modelName": name, "error": e.to_string() }),
            ),
        }
    });
}

// ---------------------------------------------------------------------------
// Summary background job
// ---------------------------------------------------------------------------

struct SummaryJob {
    state: SharedState,
    meeting_id: String,
    text: String,
    provider_id: String,
    model_name: String,
    custom_prompt: String,
    template_id: String,
    summary_language: Option<String>,
}

impl SummaryJob {
    async fn run(self) {
        let pool = self.state.pool();
        let started = Instant::now();
        match self.generate().await {
            Ok((markdown, chunk_count)) => {
                if self.was_cancelled().await {
                    log::info!("[server] discarding summary for cancelled {}", self.meeting_id);
                    return;
                }
                if let Some(name) = crate::summary::extract_meeting_name_from_markdown(&markdown) {
                    let _ =
                        MeetingsRepository::update_meeting_name(pool, &self.meeting_id, &name).await;
                }
                if let Err(e) = SummaryProcessesRepository::update_process_completed(
                    pool,
                    &self.meeting_id,
                    json!({ "markdown": markdown }),
                    chunk_count,
                    started.elapsed().as_secs_f64(),
                )
                .await
                {
                    log::error!("[server] failed to store summary: {}", e);
                }
            }
            Err(e) => {
                log::error!("[server] summary generation failed: {}", e);
                let _ =
                    SummaryProcessesRepository::update_process_failed(pool, &self.meeting_id, &e)
                        .await;
            }
        }
    }

    /// The user may have cancelled while the LLM was still working.
    async fn was_cancelled(&self) -> bool {
        match SummaryProcessesRepository::get_summary_data_for_meeting(
            self.state.pool(),
            &self.meeting_id,
        )
        .await
        {
            Ok(Some(process)) => serde_json::to_value(&process)
                .ok()
                .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(|s| s.to_lowercase()))
                .map(|s| s == "cancelled")
                .unwrap_or(false),
            _ => false,
        }
    }

    async fn generate(&self) -> Result<(String, i64), String> {
        let pool = self.state.pool();
        let provider = LLMProvider::from_str(&self.provider_id)?;

        if matches!(provider, LLMProvider::BuiltInAI) {
            return Err(
                "The bundled local model is not available in the self-hosted server. Pick Ollama or a cloud provider in Settings."
                    .to_string(),
            );
        }

        let mut api_key = match provider {
            LLMProvider::Ollama | LLMProvider::BuiltInAI | LLMProvider::CustomOpenAI => {
                String::new()
            }
            _ => SettingsRepository::get_api_key(pool, &self.provider_id)
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or_default(),
        };

        let setting = SettingsRepository::get_model_config(pool)
            .await
            .map_err(|e| e.to_string())?
            .and_then(|s| serde_json::to_value(&s).ok())
            .unwrap_or(Value::Null);
        let ollama_endpoint = self
            .state
            .config
            .ollama_endpoint
            .clone()
            .or_else(|| pick(&setting, "ollamaEndpoint").as_str().map(|s| s.to_string()));

        let custom_openai = SettingsRepository::get_custom_openai_config(pool)
            .await
            .map_err(|e| e.to_string())?;
        let custom_endpoint = custom_openai.as_ref().map(|c| c.endpoint.clone());
        if matches!(provider, LLMProvider::CustomOpenAI) {
            if let Some(key) = custom_openai.as_ref().and_then(|c| c.api_key.clone()) {
                api_key = key;
            }
        }

        let template = templates::get_template(&self.template_id)?;

        // ponytail: fixed context budgets instead of querying each model's window.
        // Raise per-provider if summaries come back truncated.
        let token_threshold = match provider {
            LLMProvider::Ollama | LLMProvider::CustomOpenAI => 24_000,
            _ => 100_000,
        };

        let (markdown, _english, chunk_count) = crate::summary::generate_meeting_summary(
            &self.state.http,
            &provider,
            &self.model_name,
            &api_key,
            &self.text,
            &self.custom_prompt,
            &self.template_id,
            &template,
            token_threshold,
            ollama_endpoint.as_deref(),
            custom_endpoint.as_deref(),
            None,
            None,
            None,
            None,
            None,
            self.summary_language.as_deref(),
            None,
            None,
        )
        .await?;

        Ok((markdown, chunk_count))
    }
}
