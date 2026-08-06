/**
 * Browser-side audio capture for the self-hosted web build.
 *
 * Replaces the desktop cpal/ScreenCaptureKit pipeline: the microphone and (on
 * Chrome/Edge) tab or screen audio are mixed in WebAudio, downsampled to 16 kHz mono
 * Int16, and streamed to `/api/audio/:session` over a WebSocket. The server handles
 * segmentation and transcription and emits `transcript-update` back over SSE.
 *
 * System audio only exists where `getDisplayMedia({audio:true})` works. Firefox and
 * Safari fall back to microphone-only, which is reported back to the caller.
 */

import { emitLocal } from './event';

/** Both engines expect 16 kHz mono. */
const TARGET_SAMPLE_RATE = 16_000;
/** 30 ms per frame at 16 kHz. */
const FRAME_SAMPLES = 480;

/**
 * AudioWorklet source, inlined as a Blob so no extra asset has to be served. It
 * derives its own decimation ratio from the context's real sample rate.
 */
const WORKLET_SOURCE = `
class MeetilyPcmProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ratio = sampleRate / ${TARGET_SAMPLE_RATE};
    this.acc = 0;
    this.sum = 0;
    this.count = 0;
    this.frame = new Int16Array(${FRAME_SAMPLES});
    this.filled = 0;
    this.paused = false;
    this.port.onmessage = (event) => {
      if (event.data && event.data.type === 'paused') this.paused = !!event.data.value;
    };
  }

  process(inputs) {
    const input = inputs[0];
    if (this.paused || !input || !input[0]) return true;
    const left = input[0];
    const right = input.length > 1 ? input[1] : null;

    for (let i = 0; i < left.length; i++) {
      this.sum += right ? (left[i] + right[i]) / 2 : left[i];
      this.count++;
      this.acc += 1;
      if (this.acc < this.ratio) continue;

      this.acc -= this.ratio;
      let value = this.count > 0 ? this.sum / this.count : 0;
      this.sum = 0;
      this.count = 0;
      if (value > 1) value = 1;
      if (value < -1) value = -1;
      this.frame[this.filled++] = value < 0 ? value * 0x8000 : value * 0x7fff;

      if (this.filled === this.frame.length) {
        const copy = this.frame.slice();
        this.filled = 0;
        this.port.postMessage(copy.buffer, [copy.buffer]);
      }
    }
    return true;
  }
}
registerProcessor('meetily-pcm', MeetilyPcmProcessor);
`;

export interface CaptureOptions {
  micDeviceName?: string | null;
  /** Ask for tab/screen audio too. Ignored where the browser does not support it. */
  systemAudio?: boolean;
  meetingName: string;
  language?: string | null;
}

export interface CaptureState {
  isRecording: boolean;
  isPaused: boolean;
  startedAt: number | null;
  pausedMs: number;
  systemAudio: boolean;
  sessionId: string | null;
}

interface Session {
  socket: WebSocket;
  context: AudioContext;
  worklet: AudioWorkletNode;
  streams: MediaStream[];
  sessionId: string;
  startedAt: number;
  pausedAt: number | null;
  pausedMs: number;
  systemAudio: boolean;
}

let session: Session | null = null;

function randomId(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

function websocketUrl(sessionId: string): string {
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${protocol}//${window.location.host}/api/audio/${sessionId}`;
}

async function loadWorklet(context: AudioContext): Promise<void> {
  const url = URL.createObjectURL(new Blob([WORKLET_SOURCE], { type: 'application/javascript' }));
  try {
    await context.audioWorklet.addModule(url);
  } finally {
    URL.revokeObjectURL(url);
  }
}

/**
 * Browsers only expose `navigator.mediaDevices` in a secure context, so serving the
 * app over plain HTTP on a LAN address leaves it `undefined` and every call site dies
 * with an opaque `TypeError`. Fail once, here, with something the operator can act on.
 */
export function mediaDevices(): MediaDevices {
  const devices = typeof navigator === 'undefined' ? undefined : navigator.mediaDevices;
  if (!devices) {
    const origin = typeof window === 'undefined' ? 'this origin' : window.location.origin;
    throw new Error(
      `The browser hides microphone access on ${origin} because it is not a secure context. ` +
        'Serve Meetily over HTTPS (and set MEETILY_COOKIE_SECURE=1), or reach it through ' +
        'http://localhost.',
    );
  }
  return devices;
}

async function openMicrophone(deviceName?: string | null): Promise<MediaStream> {
  // The existing UI passes device *names*; map back to an id where possible.
  let deviceId: string | undefined;
  if (deviceName) {
    const devices = await mediaDevices().enumerateDevices();
    deviceId = devices.find((d) => d.kind === 'audioinput' && d.label === deviceName)?.deviceId;
  }
  return mediaDevices().getUserMedia({
    audio: {
      deviceId: deviceId ? { exact: deviceId } : undefined,
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    },
    video: false,
  });
}

async function openSystemAudio(): Promise<MediaStream | null> {
  const media = mediaDevices() as MediaDevices & {
    getDisplayMedia?: (constraints: MediaStreamConstraints) => Promise<MediaStream>;
  };
  if (typeof media.getDisplayMedia !== 'function') return null;
  try {
    // Video is required by the spec even though only audio is used; the track is
    // stopped immediately so no frames get encoded.
    const stream = await media.getDisplayMedia({ audio: true, video: true });
    stream.getVideoTracks().forEach((track) => track.stop());
    if (stream.getAudioTracks().length === 0) {
      stream.getTracks().forEach((track) => track.stop());
      return null;
    }
    return stream;
  } catch (error) {
    console.warn('[web-shim] system audio unavailable, continuing with microphone only', error);
    return null;
  }
}

export function isCapturing(): boolean {
  return session !== null;
}

export function getCaptureState(): CaptureState {
  if (!session) {
    return {
      isRecording: false,
      isPaused: false,
      startedAt: null,
      pausedMs: 0,
      systemAudio: false,
      sessionId: null,
    };
  }
  return {
    isRecording: true,
    isPaused: session.pausedAt !== null,
    startedAt: session.startedAt,
    pausedMs: session.pausedMs + (session.pausedAt ? Date.now() - session.pausedAt : 0),
    systemAudio: session.systemAudio,
    sessionId: session.sessionId,
  };
}

export async function listInputDevices(): Promise<MediaDeviceInfo[]> {
  const devices = await mediaDevices().enumerateDevices();
  return devices.filter((device) => device.kind === 'audioinput');
}

export async function startCapture(options: CaptureOptions): Promise<{ systemAudio: boolean }> {
  if (session) throw new Error('Recording already in progress');

  const streams: MediaStream[] = [];
  let context: AudioContext | null = null;
  try {
    const mic = await openMicrophone(options.micDeviceName);
    streams.push(mic);

    const system = options.systemAudio ? await openSystemAudio() : null;
    if (system) streams.push(system);

    // Ask for 16 kHz directly; browsers that refuse keep their native rate and the
    // worklet decimates instead.
    try {
      context = new AudioContext({ sampleRate: TARGET_SAMPLE_RATE });
    } catch {
      context = new AudioContext();
    }
    await loadWorklet(context);

    const mixer = context.createGain();
    mixer.gain.value = 1;
    context.createMediaStreamSource(mic).connect(mixer);
    if (system) {
      // Duck the far end slightly so it cannot bury the local speaker.
      const systemGain = context.createGain();
      systemGain.gain.value = 0.8;
      context.createMediaStreamSource(system).connect(systemGain);
      systemGain.connect(mixer);
    }

    const worklet = new AudioWorkletNode(context, 'meetily-pcm');
    mixer.connect(worklet);
    // Keep the graph pulling without echoing audio back to the speakers.
    const mute = context.createGain();
    mute.gain.value = 0;
    worklet.connect(mute).connect(context.destination);

    const sessionId = randomId();
    const socket = new WebSocket(websocketUrl(sessionId));
    socket.binaryType = 'arraybuffer';

    await new Promise<void>((resolve, reject) => {
      socket.onopen = () => resolve();
      socket.onerror = () => reject(new Error('Failed to reach the transcription server'));
    });

    socket.send(
      JSON.stringify({
        type: 'start',
        meetingName: options.meetingName,
        language: options.language ?? null,
      }),
    );

    worklet.port.onmessage = (message) => {
      if (socket.readyState === WebSocket.OPEN) socket.send(message.data as ArrayBuffer);
    };

    socket.onclose = () => {
      if (session && session.sessionId === sessionId) {
        console.warn('[web-shim] transcription socket closed, stopping capture');
        void stopCapture();
      }
    };

    session = {
      socket,
      context,
      worklet,
      streams,
      sessionId,
      startedAt: Date.now(),
      pausedAt: null,
      pausedMs: 0,
      systemAudio: Boolean(system),
    };

    return { systemAudio: Boolean(system) };
  } catch (error) {
    streams.forEach((stream) => stream.getTracks().forEach((track) => track.stop()));
    if (context) await context.close().catch(() => undefined);
    throw error instanceof Error ? error : new Error(String(error));
  }
}

export async function stopCapture(): Promise<void> {
  const active = session;
  if (!active) return;
  session = null;

  try {
    if (active.socket.readyState === WebSocket.OPEN) {
      active.socket.onclose = null;
      active.socket.send(JSON.stringify({ type: 'stop' }));
    }
  } catch (error) {
    console.warn('[web-shim] failed to send stop frame', error);
  }

  active.worklet.port.onmessage = null;
  active.streams.forEach((stream) => stream.getTracks().forEach((track) => track.stop()));
  try {
    active.worklet.disconnect();
    await active.context.close();
  } catch (error) {
    console.warn('[web-shim] audio context teardown failed', error);
  }

  // Give the server a moment to flush the tail before dropping the socket.
  setTimeout(() => {
    try {
      active.socket.close();
    } catch {
      /* already closed */
    }
  }, 1_000);
}

export function pauseCapture(): void {
  if (!session || session.pausedAt !== null) return;
  session.pausedAt = Date.now();
  session.worklet.port.postMessage({ type: 'paused', value: true });
  emitLocal('recording-paused', { message: 'Recording paused' });
}

export function resumeCapture(): void {
  if (!session || session.pausedAt === null) return;
  session.pausedMs += Date.now() - session.pausedAt;
  session.pausedAt = null;
  session.worklet.port.postMessage({ type: 'paused', value: false });
  emitLocal('recording-resumed', { message: 'Recording resumed' });
}
