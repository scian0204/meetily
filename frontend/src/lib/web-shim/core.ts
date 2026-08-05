/**
 * Drop-in replacement for `@tauri-apps/api/core` in the self-hosted web build.
 *
 * `invoke()` routes in three tiers:
 *   1. browser-local commands (recording, devices, analytics, onboarding, folders)
 *   2. everything else -> `POST /api/invoke/:cmd` on the server
 *   3. unsupported -> throw, loudly. Never a silent no-op.
 *
 * Tier 1 exists because those commands drive hardware or OS surfaces that only make
 * sense on the client (or nowhere). Tier 2 is the real port of the Tauri command set.
 */

import {
  getCaptureState,
  isCapturing,
  listInputDevices,
  pauseCapture,
  resumeCapture,
  startCapture,
  stopCapture,
} from './capture';
import { emitLocal } from './event';

export const isWebBuild = true;

// Two hooks gate behaviour on Tauri's window globals (useRecordingStateSync.ts:50,
// usePlatform.ts:40). Declaring them keeps those branches live; platform detection
// then resolves through the aliased plugin shim.
if (typeof window !== 'undefined') {
  const marker = window as unknown as Record<string, unknown>;
  marker.__TAURI__ = marker.__TAURI__ ?? { web: true };
  marker.__TAURI_INTERNALS__ = marker.__TAURI_INTERNALS__ ?? { web: true };
}

type Args = Record<string, unknown>;
type LocalHandler = (args: Args) => unknown | Promise<unknown>;

const ONBOARDING_KEY = 'meetily_onboarding_status';

function readJson<T>(key: string): T | null {
  if (typeof window === 'undefined') return null;
  try {
    const raw = window.localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : null;
  } catch {
    return null;
  }
}

function writeJson(key: string, value: unknown): void {
  if (typeof window === 'undefined') return;
  try {
    window.localStorage.setItem(key, JSON.stringify(value));
  } catch (error) {
    console.warn(`[web-shim] could not persist ${key}`, error);
  }
}

function language(): string | null {
  if (typeof window === 'undefined') return null;
  return window.localStorage.getItem('primaryLanguage');
}

function meetingName(args: Args): string {
  const provided = (args.meeting_name ?? args.meetingName) as string | undefined;
  if (provided && provided.trim()) return provided;
  const now = new Date();
  const two = (n: number) => String(n).padStart(2, '0');
  return `Meeting ${two(now.getDate())}_${two(now.getMonth() + 1)}_${two(
    now.getFullYear() % 100,
  )}_${two(now.getHours())}_${two(now.getMinutes())}_${two(now.getSeconds())}`;
}

/** Upload an audio file for server-side transcription. */
export async function uploadAudioFile(file: File, name?: string): Promise<void> {
  const form = new FormData();
  form.append('file', file);
  if (name) form.append('meeting_name', name);
  const response = await fetch('/api/upload', {
    method: 'POST',
    body: form,
    credentials: 'same-origin',
  });
  if (!response.ok) {
    const body = (await response.json().catch(() => null)) as { error?: string } | null;
    throw new Error(body?.error ?? `Upload failed with status ${response.status}`);
  }
}

async function pickAudioFile(): Promise<File | null> {
  return new Promise((resolve) => {
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = 'audio/*,video/mp4,video/webm,.m4a,.wav,.mp3,.flac,.ogg';
    input.onchange = () => resolve(input.files?.[0] ?? null);
    input.oncancel = () => resolve(null);
    input.click();
  });
}

function unsupported(cmd: string, reason: string): never {
  const message = `${cmd} is not available in the self-hosted web build: ${reason}`;
  console.warn(`[web-shim] ${message}`);
  throw new Error(message);
}

// ---------------------------------------------------------------------------
// Tier 1: browser-local commands
// ---------------------------------------------------------------------------

const noop = async () => null;

const localHandlers: Record<string, LocalHandler> = {
  // --- recording ---
  start_recording: async (args) => {
    await startCapture({
      micDeviceName: (args.mic_device_name as string) ?? null,
      systemAudio: true,
      meetingName: meetingName(args),
      language: language(),
    });
    return null;
  },
  start_recording_with_meeting_name: async (args) => {
    await startCapture({
      micDeviceName: null,
      systemAudio: true,
      meetingName: meetingName(args),
      language: language(),
    });
    return null;
  },
  start_recording_with_devices_and_meeting: async (args) => {
    await startCapture({
      micDeviceName: (args.mic_device_name as string) ?? null,
      // A null system device means "default" on desktop; in the browser we still try
      // for tab audio and quietly fall back to microphone-only.
      systemAudio: args.system_device_name !== '__none__',
      meetingName: meetingName(args),
      language: language(),
    });
    return null;
  },
  stop_recording: async () => {
    await stopCapture();
    return null;
  },
  pause_recording: async () => {
    pauseCapture();
    return null;
  },
  resume_recording: async () => {
    resumeCapture();
    return null;
  },
  is_recording: async () => isCapturing(),
  get_recording_state: async () => {
    const state = getCaptureState();
    const elapsed = state.startedAt ? (Date.now() - state.startedAt) / 1000 : null;
    return {
      is_recording: state.isRecording,
      is_paused: state.isPaused,
      is_active: state.isRecording && !state.isPaused,
      recording_duration: elapsed,
      active_duration: elapsed === null ? null : elapsed - state.pausedMs / 1000,
      total_pause_duration: state.pausedMs / 1000,
      current_pause_duration: null,
    };
  },
  // The server names the meeting from the start frame; the client already knows it.
  get_recording_meeting_name: noop,
  get_meeting_folder_path: noop,
  // Transcripts live in React state during a web recording, and the poll loop in
  // useRecordingStop only needs an idle status to proceed.
  get_transcript_history: async () => [],
  get_transcription_status: async () => ({
    chunks_in_queue: 0,
    is_processing: false,
    last_activity_ms: 0,
  }),
  has_audio_checkpoints: async () => false,
  cleanup_checkpoints: noop,

  // --- devices ---
  get_audio_devices: async () => {
    const devices = await listInputDevices();
    const names = devices
      .map((device, index) => device.label || `Microphone ${index + 1}`)
      .filter((name, index, all) => all.indexOf(name) === index);
    return names.length > 0 ? names : ['Default Microphone'];
  },
  start_audio_level_monitoring: noop,
  stop_audio_level_monitoring: noop,
  is_audio_level_monitoring: async () => false,
  get_active_audio_output: noop,
  get_audio_backend_info: async () => null,
  get_current_audio_backend: async () => 'browser',
  set_audio_backend: noop,

  // --- permissions: the browser prompts on getUserMedia instead ---
  trigger_microphone_permission: async () => {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    stream.getTracks().forEach((track) => track.stop());
    return true;
  },
  trigger_system_audio_permission_command: async () => true,
  open_system_settings: noop,

  // --- onboarding: per-browser, since the server has no first-launch state ---
  check_first_launch: async () => readJson(ONBOARDING_KEY) === null,
  get_onboarding_status: async () => readJson(ONBOARDING_KEY),
  save_onboarding_status_cmd: async (args) => {
    const status = (args.status as Record<string, unknown>) ?? {};
    writeJson(ONBOARDING_KEY, { ...status, last_updated: new Date().toISOString() });
    return null;
  },
  complete_onboarding: async (args) => {
    writeJson(ONBOARDING_KEY, {
      version: '1.0',
      completed: true,
      current_step: 4,
      model_status: {
        parakeet: 'downloaded',
        summary: 'downloaded',
        selected_summary_model: args.model ?? null,
      },
      last_updated: new Date().toISOString(),
    });
    return null;
  },
  reset_onboarding_status_cmd: async () => {
    if (typeof window !== 'undefined') window.localStorage.removeItem(ONBOARDING_KEY);
    return null;
  },

  // --- audio import: the native picker becomes a file input plus an upload ---
  select_and_validate_audio_command: async () => {
    const file = await pickAudioFile();
    if (!file) return null;
    const name = file.name.replace(/\.[^.]+$/, '');
    await uploadAudioFile(file, name);
    emitLocal('upload-started', { meeting_name: name });
    return { path: file.name, name: file.name, size: file.size, duration: null };
  },
  validate_audio_file_command: async () => ({ valid: true }),
  start_import_audio_command: async () =>
    unsupported(
      'start_import_audio_command',
      'the upload already starts transcription on the server',
    ),
  cancel_import_command: noop,

  // --- desktop shells with no browser equivalent ---
  open_external_url: async (args) => {
    const url = args.url as string | undefined;
    if (url) window.open(url, '_blank', 'noopener,noreferrer');
    return null;
  },
  open_database_folder: noop,
  open_models_folder: noop,
  open_recordings_folder: noop,
  open_parakeet_models_folder: noop,
  open_meeting_folder: noop,
  toggle_console: noop,
  show_console: noop,
  hide_console: noop,

  // --- desktop-only data migration ---
  select_legacy_database_path: async () =>
    unsupported('select_legacy_database_path', 'copy the old .db into the server data volume'),
  import_and_initialize_database: async () =>
    unsupported('import_and_initialize_database', 'copy the old .db into the server data volume'),

  // --- the bundled llama.cpp sidecar is not shipped in the server image ---
  builtin_ai_download_model: async () =>
    unsupported('builtin_ai_download_model', 'use Ollama or a cloud provider for summaries'),
  builtin_ai_cancel_download: noop,
  builtin_ai_delete_model: noop,
  builtin_ai_get_recommended_model: async () => null,
  builtin_ai_get_models_directory: async () => null,

  // --- analytics: no telemetry from a self-hosted deployment ---
  init_analytics: noop,
  identify_user: noop,
  is_analytics_enabled: async () => false,
  disable_analytics: noop,
  start_analytics_session: noop,
  end_analytics_session: noop,
  is_analytics_session_active: async () => false,
};

function localHandlerFor(cmd: string): LocalHandler | undefined {
  if (localHandlers[cmd]) return localHandlers[cmd];
  if (cmd.startsWith('track_')) return noop;
  return undefined;
}

// ---------------------------------------------------------------------------
// Tier 2: HTTP dispatch
// ---------------------------------------------------------------------------

export async function invoke<T = unknown>(cmd: string, args?: Args): Promise<T> {
  const local = localHandlerFor(cmd);
  if (local) return (await local(args ?? {})) as T;

  const response = await fetch(`/api/invoke/${cmd}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    credentials: 'same-origin',
    body: JSON.stringify(args ?? {}),
  });

  if (response.status === 401) {
    if (typeof window !== 'undefined' && !window.location.pathname.startsWith('/login')) {
      window.location.href = '/login';
    }
    throw new Error('Not authenticated');
  }

  const body = (await response.json().catch(() => null)) as { ok?: unknown; error?: string } | null;

  if (!response.ok) {
    // Keep the plain-string message: several UI paths display it verbatim.
    throw new Error(body?.error ?? `${cmd} failed with status ${response.status}`);
  }
  return (body?.ok ?? null) as T;
}

/** Tauri's asset protocol helper. Served paths are already URLs here. */
export function convertFileSrc(filePath: string, _protocol?: string): string {
  return filePath;
}
