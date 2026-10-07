import { describe, expect, mock, setSystemTime, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

// The runtime and worklet are plain browser scripts; evaluate them against fake globals.
const RUNTIME = readFileSync(join(import.meta.dir, '../../web-runtime/runtime.js'), 'utf8');
const WORKLET = readFileSync(join(import.meta.dir, '../../web-runtime/audio-worklet.js'), 'utf8');

const tick = (ms = 0) => new Promise((resolve) => setTimeout(resolve, ms));

async function until(check: () => unknown, what = 'condition') {
  for (let i = 0; i < 200; i++) {
    if (check()) return;
    await tick(5);
  }
  throw new Error(`timed out waiting for ${what}`);
}

type Listeners = Record<string, Array<(e: any) => void>>;

function on(listeners: Listeners, type: string, fn: (e: any) => void) {
  (listeners[type] ||= []).push(fn);
}

function fire(listeners: Listeners, type: string, event: any = {}) {
  for (const fn of [...(listeners[type] || [])]) fn(event);
}

class FakeElement {
  style: any = {};
  textContent = '';
  children: FakeElement[] = [];
  attributes: Record<string, string> = {};
  listeners: Listeners = {};
  removed = false;
  files: any[] = [];
  [key: string]: any;
  constructor(public tag: string, private doc: FakeDocument) {}
  setAttribute(name: string, value: string) {
    this.attributes[name] = value;
  }
  appendChild(child: FakeElement) {
    this.children.push(child);
  }
  append(...children: FakeElement[]) {
    this.children.push(...children);
  }
  remove() {
    this.removed = true;
  }
  addEventListener(type: string, fn: (e: any) => void) {
    on(this.listeners, type, fn);
  }
  fire(type: string, event: any = {}) {
    fire(this.listeners, type, event);
  }
  click() {
    this.doc.onClick(this);
  }
}

class FakeDocument {
  readyState = 'complete';
  created: FakeElement[] = [];
  listeners: Listeners = {};
  body = new FakeElement('body', this);
  onClick: (el: FakeElement) => void = () => {};
  createElement(tag: string) {
    const el = new FakeElement(tag, this);
    this.created.push(el);
    return el;
  }
  addEventListener(type: string, fn: (e: any) => void) {
    on(this.listeners, type, fn);
  }
  live() {
    return this.created.filter((el) => el.tag === 'div' && !el.removed);
  }
}

const fakeTrack = (kind: string, deviceId?: string) => ({
  kind,
  stopped: false,
  stop() {
    this.stopped = true;
  },
  getSettings: () => ({ deviceId }),
});

const fakeStream = (tracks: any[]) => ({
  getTracks: () => tracks,
  getAudioTracks: () => tracks.filter((t) => t.kind === 'audio'),
  getVideoTracks: () => tracks.filter((t) => t.kind === 'video'),
});

interface EnvOptions {
  devices?: any[];
  storage?: Record<string, string>;
  sessionStatus?: number;
  micError?: any;
}

function makeEnv(opts: EnvOptions = {}) {
  const sockets: any[] = [];
  class FakeWebSocket {
    readyState = 0;
    bufferedAmount = 0;
    binaryType = 'blob';
    sent: any[] = [];
    onopen: any;
    onmessage: any;
    onclose: any;
    constructor(public url: string) {
      sockets.push(this);
    }
    send(data: any) {
      this.sent.push(typeof data === 'string' ? JSON.parse(data) : data);
    }
    close() {
      this.readyState = 3;
      this.onclose?.({ code: 1005 });
    }
    serverOpen() {
      this.readyState = 1;
      this.onopen?.({});
    }
    serverClose(code = 1006) {
      this.readyState = 3;
      this.onclose?.({ code });
    }
    serverSend(msg: any) {
      this.onmessage?.({ data: JSON.stringify(msg) });
    }
    hello(boot = 'boot-1', seq = 0) {
      this.serverSend({ t: 'hello', boot, seq });
    }
  }

  const calls: string[] = [];
  const micTrack = fakeTrack('audio', 'usb');
  const tabTrack = fakeTrack('audio');
  const videoTrack = fakeTrack('video');
  const contexts: any[] = [];
  const nodes: any[] = [];
  const xhrs: any[] = [];

  class FakeAudioContext {
    state = 'running';
    closed = false;
    destination = {};
    sources: any[] = [];
    audioWorklet = { addModule: mock(async (_url: string) => {}) };
    constructor(public options: any) {
      contexts.push(this);
    }
    createMediaStreamSource(stream: any) {
      const source = { stream, connect: (_node: any, _output: number, input: number) => this.sources.push({ stream, input }) };
      return source;
    }
    createGain() {
      return { gain: { value: 1 }, connect: (next: any) => next };
    }
    async resume() {}
    async close() {
      this.closed = true;
    }
  }

  class FakeWorkletNode {
    port: any = {
      onmessage: null,
      postMessage: (msg: any) => {
        if (msg !== 'flush') return;
        this.port.onmessage({ data: new ArrayBuffer(3) }); // partial frame
        this.port.onmessage({ data: 'flushed' });
      },
    };
    constructor(public ctx: any, public name: string, public options: any) {
      nodes.push(this);
    }
    connect(next: any) {
      return next;
    }
  }

  class FakeXHR {
    upload: any = {};
    status = 0;
    responseText = '';
    method = '';
    url = '';
    body: any;
    onload: any;
    onerror: any;
    constructor() {
      xhrs.push(this);
    }
    open(method: string, url: string) {
      this.method = method;
      this.url = url;
    }
    send(body: any) {
      this.body = body;
    }
    respond(status: number, body: any) {
      this.upload.onprogress?.({ lengthComputable: true, loaded: 1, total: 2 });
      this.status = status;
      this.responseText = JSON.stringify(body);
      this.onload();
    }
  }

  const storage = new Map(Object.entries(opts.storage || {}));
  const windowListeners: Listeners = {};
  const doc = new FakeDocument();
  const win: any = {
    document: doc,
    crypto: globalThis.crypto,
    location: { protocol: 'https:', host: 'meetily.test', pathname: '/settings', search: '?x=1', href: '', reload: mock(() => {}) },
    WebSocket: FakeWebSocket,
    fetch: mock(async () => ({ status: opts.sessionStatus ?? 200 })),
    setTimeout: (fn: () => void, ms = 0) => setTimeout(fn, Math.min(ms, 5)), // fast backoff/timeouts
    clearTimeout,
    localStorage: {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => storage.set(key, value),
    },
    navigator: {
      mediaDevices: {
        enumerateDevices: async () => opts.devices ?? [],
        getUserMedia: mock(async (_constraints: any) => {
          calls.push('getUserMedia');
          if (opts.micError) throw opts.micError;
          return fakeStream([micTrack]);
        }),
        getDisplayMedia: mock(async (_constraints: any) => {
          calls.push('getDisplayMedia');
          return fakeStream([videoTrack, tabTrack]);
        }),
      },
    },
    open: mock(() => null),
    addEventListener: (type: string, fn: (e: any) => void) => on(windowListeners, type, fn),
    removeEventListener: (type: string, fn: (e: any) => void) => {
      windowListeners[type] = (windowListeners[type] || []).filter((f) => f !== fn);
    },
    AudioContext: FakeAudioContext,
    AudioWorkletNode: FakeWorkletNode,
    MediaStream: class {
      constructor(public tracks: any[]) {}
    },
    XMLHttpRequest: FakeXHR,
  };

  new Function('window', RUNTIME)(win);
  const T = win.__TAURI_INTERNALS__;
  const socketsAt = (path: string) => sockets.filter((s) => new URL(s.url).pathname === path);
  const byPath = (path: string) => () => socketsAt(path).at(-1);
  const control = byPath('/api/ws');
  const invokeOf = (ws: any, cmd: string) => ws.sent.find((m: any) => m.t === 'invoke' && m.cmd === cmd);
  const reply = (ws: any, cmd: string, result: any) => ws.serverSend({ t: 'result', id: invokeOf(ws, cmd).id, ...result });

  // Open the control socket (server hello first) and answer the runtime's startup get_recording_state.
  function connect(recordingState: any = { is_recording: false }, boot = 'boot-1', seq = 0) {
    const ws = control();
    ws.serverOpen();
    ws.hello(boot, seq);
    reply(ws, 'get_recording_state', { ok: true, value: recordingState });
    return ws;
  }

  // Wait for the runtime to replace a dropped socket.
  async function reconnected(old: any, path = '/api/ws') {
    await until(() => byPath(path)() !== old, 'reconnect');
    return byPath(path)();
  }

  // Start a mic-only recording, answer the server's start, open the audio socket.
  async function record(ws: any) {
    const cmd = 'start_recording_with_devices_and_meeting';
    const start = T.invoke(cmd, { micDeviceName: null, systemDeviceName: 'None (microphone only) (output)', meetingName: 'M' });
    await until(() => invokeOf(ws, cmd), 'start invoke');
    reply(ws, cmd, { ok: true, value: null });
    await start;
    const audio = byPath('/api/audio')();
    audio.serverOpen();
    return audio;
  }

  // Same calls @tauri-apps/api/event's listen() makes.
  async function listen(event: string, fn: (e: any) => void) {
    return T.invoke('plugin:event|listen', { event, target: { kind: 'Any' }, handler: T.transformCallback(fn) });
  }

  return {
    win, T, doc, storage, calls, micTrack, tabTrack, videoTrack, contexts, nodes, xhrs, windowListeners,
    control, audio: byPath('/api/audio'), socketsAt, invokeOf, reply, connect, reconnected, record, listen,
  };
}

describe('bootstrap', () => {
  test('is a no-op inside the real Tauri webview and outside a browser', () => {
    const real = { real: true };
    const win: any = {
      __TAURI_INTERNALS__: real,
      WebSocket: class {
        constructor() {
          throw new Error('must not connect');
        }
      },
    };
    new Function('window', RUNTIME)(win);
    expect(win.__TAURI_INTERNALS__).toBe(real);
    expect(win.__TAURI_OS_PLUGIN_INTERNALS__).toBeUndefined();
    expect(() => new Function('window', RUNTIME)(undefined)).not.toThrow();
  });

  test('installs the Tauri globals and opens the control socket on the page origin', () => {
    const env = makeEnv();
    expect(env.T.metadata.currentWebview).toEqual({ windowLabel: 'main', label: 'main' });
    expect(env.T.plugins.path).toEqual({ sep: '/', delimiter: ':' });
    expect(env.win.__TAURI_OS_PLUGIN_INTERNALS__.platform).toBe('linux');
    expect(env.win.__TAURI__).toBeUndefined();
    expect(env.control().url).toBe('wss://meetily.test/api/ws');
  });
});

describe('invoke', () => {
  test('round-trips over the control socket with args unchanged', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const call = env.T.invoke('api_get_meeting', { meetingId: 'm1' });
    expect(env.invokeOf(ws, 'api_get_meeting')).toEqual({
      t: 'invoke',
      id: expect.any(String),
      cmd: 'api_get_meeting',
      args: { meetingId: 'm1' },
    });
    env.reply(ws, 'api_get_meeting', { ok: true, value: { id: 'm1' } });
    expect(await call).toEqual({ id: 'm1' });

    const unit = env.T.invoke('api_save_meeting_title', { meetingId: 'm1', title: 't' });
    env.reply(ws, 'api_save_meeting_title', { ok: true });
    expect(await unit).toBeNull();
  });

  test('rejects with the raw error value', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const call = env.T.invoke('is_recording');
    env.reply(ws, 'is_recording', { ok: false, error: 'No recording in progress' });
    expect(await call.catch((e: unknown) => e)).toBe('No recording in progress');
  });

  test('queues invokes until the socket opens', async () => {
    const env = makeEnv();
    const call = env.T.invoke('api_get_meetings');
    expect(env.control().sent).toEqual([]);
    const ws = env.connect();
    expect(ws.sent.some((m: any) => m.t === 'resume')).toBe(false);
    env.reply(ws, 'api_get_meetings', { ok: true, value: [] });
    expect(await call).toEqual([]);
  });

  test('resumes in-flight invokes and re-sends listens after a reconnect', async () => {
    const env = makeEnv();
    const ws1 = env.connect();
    await env.listen('transcript-update', () => {});
    const download = env.T.invoke('parakeet_download_model', { modelName: 'm' });
    const { id } = env.invokeOf(ws1, 'parakeet_download_model');

    ws1.serverClose(); // connection drops
    const queued = env.T.invoke('api_get_meetings');
    const ws2 = await env.reconnected(ws1);
    ws2.serverOpen();
    ws2.hello();

    expect(ws2.sent).toEqual([
      { t: 'listen', event: 'recording-stopped' },
      { t: 'listen', event: 'transcript-update' },
      { t: 'resume', ids: [id] },
      { t: 'invoke', id: expect.any(String), cmd: 'api_get_meetings', args: {} },
    ]);
    ws2.serverSend({ t: 'result', id, ok: true, value: 'done' });
    env.reply(ws2, 'api_get_meetings', { ok: true, value: [] });
    expect(await download).toBe('done');
    expect(await queued).toEqual([]);
  });

  test('a socket that never opens checks the session and redirects to login on 401', async () => {
    const env = makeEnv({ sessionStatus: 401 });
    env.control().serverClose();
    await until(() => env.win.location.href, 'redirect');
    expect(env.win.fetch).toHaveBeenCalledWith('/api/session', { credentials: 'same-origin' });
    expect(env.win.location.href).toBe('/login?next=' + encodeURIComponent('/settings?x=1'));
  });
});

describe('events', () => {
  test('listen, dispatch and unlisten', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const got: any[] = [];
    const eventId = await env.listen('transcript-update', (e) => got.push(e));
    await env.listen('transcript-update', () => {});
    expect(ws.sent.filter((m: any) => m.t === 'listen' && m.event === 'transcript-update')).toHaveLength(1);

    ws.serverSend({ t: 'event', seq: 1, event: 'transcript-update', payload: { text: 'hi' } });
    expect(got).toEqual([{ event: 'transcript-update', id: eventId, payload: { text: 'hi' } }]);

    // What the unlisten function returned by @tauri-apps/api/event's listen() does.
    env.win.__TAURI_EVENT_PLUGIN_INTERNALS__.unregisterListener('transcript-update', eventId);
    expect(await env.T.invoke('plugin:event|unlisten', { event: 'transcript-update', eventId })).toBeNull();
    ws.serverSend({ t: 'event', seq: 2, event: 'transcript-update', payload: { text: 'again' } });
    expect(got).toHaveLength(1);
  });

  test('emit goes to the server only; tauri:// events stay local', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const config: any[] = [];
    const drops: any[] = [];
    await env.listen('model-config-updated', (e) => config.push(e));
    await env.listen('tauri://drag-drop', (e) => drops.push(e.payload));

    await env.T.invoke('plugin:event|emit', { event: 'model-config-updated', payload: { provider: 'ollama' } });
    expect(ws.sent).toContainEqual({ t: 'emit', event: 'model-config-updated', payload: { provider: 'ollama' } });
    expect(config).toEqual([]); // the server's echo delivers it

    await env.T.invoke('plugin:event|emit', { event: 'tauri://drag-drop', payload: { paths: ['a.wav'] } });
    expect(drops).toEqual([{ paths: ['a.wav'] }]);
    expect(ws.sent.filter((m: any) => String(m.event).startsWith('tauri://'))).toEqual([]);
  });
});

describe('local commands', () => {
  test('get_audio_devices lists browser microphones plus tab audio', async () => {
    const env = makeEnv({
      devices: [
        { kind: 'audioinput', deviceId: 'default', label: 'Default - USB Mic' },
        { kind: 'audioinput', deviceId: 'communications', label: 'Communications - USB Mic' },
        { kind: 'audioinput', deviceId: 'usb', label: 'USB Mic' },
        { kind: 'audioinput', deviceId: 'usb2', label: 'USB Mic' },
        { kind: 'audioinput', deviceId: 'x', label: '' },
        { kind: 'audiooutput', deviceId: 'spk', label: 'Speakers' },
      ],
    });
    expect(await env.T.invoke('get_audio_devices')).toEqual([
      { name: 'USB Mic', device_type: 'Input' },
      { name: 'USB Mic #2', device_type: 'Input' },
      { name: 'Microphone 3', device_type: 'Input' },
      { name: 'Browser Tab Audio', device_type: 'Output' },
      { name: 'None (microphone only)', device_type: 'Output' },
    ]);
  });

  test('get_audio_devices always reports a microphone', async () => {
    const env = makeEnv({ devices: [] });
    expect(await env.T.invoke('get_audio_devices')).toEqual([
      { name: 'Microphone', device_type: 'Input' },
      { name: 'Browser Tab Audio', device_type: 'Output' },
      { name: 'None (microphone only)', device_type: 'Output' },
    ]);
  });

  test('updater, process, analytics and OS commands never reach the server', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const { T, win } = env;
    expect(await T.invoke('plugin:updater|check', {})).toBeNull();
    expect(await T.invoke('plugin:updater|download_and_install', {}).catch((e: unknown) => e)).toBe(
      'Updates are not available in the web version'
    );
    expect(await T.invoke('plugin:process|restart')).toBeNull();
    expect(win.location.reload).toHaveBeenCalledTimes(1);
    expect(await T.invoke('plugin:process|exit', { code: 0 })).toBeNull();
    for (const cmd of ['init_analytics', 'identify_user', 'track_event', 'track_meeting_started', 'end_analytics_session']) {
      expect(await T.invoke(cmd, {})).toBeNull();
    }
    expect(await T.invoke('is_analytics_enabled')).toBe(false);
    expect(await T.invoke('trigger_system_audio_permission_command')).toBe(true);
    expect(await T.invoke('get_active_audio_output')).toEqual({
      device_name: 'Browser',
      is_bluetooth: false,
      sample_rate: null,
      device_type: 'Unknown',
    });
    expect(await T.invoke('trigger_microphone_permission')).toBe(true);
    expect(await T.invoke('check_homebrew_database', { path: '/usr/local/var/meetily/meeting_minutes.db' })).toBeNull();
    await T.invoke('open_external_url', { url: 'https://example.com' });
    await T.invoke('open_meeting_folder', { meetingId: 'a/b' });
    expect(win.open.mock.calls).toEqual([
      ['https://example.com', '_blank', 'noopener'],
      ['/api/meetings/a%2Fb/files/', '_blank'],
    ]);
    expect(ws.sent.filter((m: any) => m.t !== 'listen' && m.cmd !== 'get_recording_state')).toEqual([]);
  });
});

describe('recording capture', () => {
  test('start: tab audio first, then the mic, then the server; stop: flush, then the server', async () => {
    const env = makeEnv({ devices: [{ kind: 'audioinput', deviceId: 'usb', label: 'USB Mic' }] });
    const ws = env.connect();
    const args = { micDeviceName: 'USB Mic (input)', systemDeviceName: 'Browser Tab Audio (output)', meetingName: 'M' };
    const start = env.T.invoke('start_recording_with_devices_and_meeting', args);
    await until(() => env.invokeOf(ws, 'start_recording_with_devices_and_meeting'), 'start invoke');

    expect(env.invokeOf(ws, 'start_recording_with_devices_and_meeting').args).toEqual(args);
    expect(env.calls).toEqual(['getDisplayMedia', 'getUserMedia']);
    expect(env.win.navigator.mediaDevices.getUserMedia.mock.calls[0][0].audio).toMatchObject({
      deviceId: { exact: 'usb' },
      echoCancellation: true,
      noiseSuppression: false,
      autoGainControl: false,
    });
    expect(env.videoTrack.stopped).toBe(true);
    const [ctx] = env.contexts;
    expect(ctx.options).toEqual({ sampleRate: 48000, latencyHint: 0.5 });
    expect(ctx.audioWorklet.addModule).toHaveBeenCalledWith('/__meetily/audio-worklet.js');
    const [node] = env.nodes;
    expect(node.options).toMatchObject({ numberOfInputs: 2, processorOptions: { mask: 3 } });
    expect(ctx.sources.map((s: any) => s.input)).toEqual([0, 1]);

    // Frames are buffered until the audio socket opens.
    const audio = env.audio();
    expect(audio.url).toMatch(/^wss:\/\/meetily\.test\/api\/audio\?capture=[0-9a-f]{32}$/);
    expect(audio.binaryType).toBe('arraybuffer');
    const frame = new ArrayBuffer(8);
    node.port.onmessage({ data: frame });
    expect(audio.sent).toEqual([]);
    audio.serverOpen();
    expect(audio.sent).toEqual([frame]);

    env.reply(ws, 'start_recording_with_devices_and_meeting', { ok: true, value: null });
    expect(await start).toBeNull();
    expect(env.storage.get('meetily.web.micDeviceId')).toBe('usb');
    expect(env.windowListeners.beforeunload).toHaveLength(1);

    const stop = env.T.invoke('stop_recording', { args: { save_path: '/data/rec.wav' } });
    await until(() => env.invokeOf(ws, 'stop_recording'), 'stop invoke');
    expect(audio.sent).toHaveLength(2); // the worklet's partial frame went out before the stop command
    expect(env.micTrack.stopped && env.tabTrack.stopped && ctx.closed).toBe(true);
    expect(audio.readyState).toBe(3);
    expect(env.windowListeners.beforeunload).toHaveLength(0);
    env.reply(ws, 'stop_recording', { ok: true, value: null });
    expect(await stop).toBeNull();
  });

  test('a microphone failure releases tab audio and rejects with a microphone error', async () => {
    const env = makeEnv({ micError: Object.assign(new Error('Permission denied'), { name: 'NotAllowedError' }) });
    const ws = env.connect();
    const err = await env.T
      .invoke('start_recording_with_devices_and_meeting', { micDeviceName: null, systemDeviceName: 'Browser Tab Audio (output)', meetingName: 'M' })
      .catch((e: unknown) => e);
    expect(typeof err).toBe('string');
    expect(err).toContain('microphone');
    expect(env.tabTrack.stopped).toBe(true);
    expect(env.invokeOf(ws, 'start_recording_with_devices_and_meeting')).toBeUndefined();
  });

  test('a rejected start releases the capture and rethrows the raw error', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const start = env.T.invoke('start_recording_with_devices_and_meeting', {
      micDeviceName: null,
      systemDeviceName: 'None (microphone only) (output)',
      meetingName: 'M',
    });
    await until(() => env.invokeOf(ws, 'start_recording_with_devices_and_meeting'), 'start invoke');
    expect(env.calls).toEqual(['getUserMedia']); // tab audio opted out
    expect(env.nodes[0].options.processorOptions).toEqual({ mask: 1 });
    env.reply(ws, 'start_recording_with_devices_and_meeting', { ok: false, error: 'Recording already in progress' });
    expect(await start.catch((e: unknown) => e)).toBe('Recording already in progress');
    expect(env.micTrack.stopped && env.contexts[0].closed).toBe(true);
  });

  test('the default system device (null) asks for tab audio, like the desktop default output', async () => {
    const env = makeEnv();
    const ws = env.connect();
    env.T.invoke('start_recording_with_devices_and_meeting', { micDeviceName: null, systemDeviceName: null, meetingName: 'M' });
    await until(() => env.invokeOf(ws, 'start_recording_with_devices_and_meeting'), 'start invoke');
    expect(env.calls).toEqual(['getDisplayMedia', 'getUserMedia']);
    expect(env.nodes[0].options.processorOptions).toEqual({ mask: 3 });
  });

  test('falls back to the device rate when the browser cannot mix 48 kHz with the microphone', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const original = env.win.AudioContext;
    env.win.AudioContext = class extends original {
      createMediaStreamSource(stream: any) {
        if (this.options && this.options.sampleRate) throw Object.assign(new Error('rate'), { name: 'NotSupportedError' });
        return super.createMediaStreamSource(stream);
      }
    };
    env.T.invoke('start_recording_with_devices_and_meeting', { micDeviceName: null, systemDeviceName: 'None (microphone only) (output)', meetingName: 'M' });
    await until(() => env.invokeOf(ws, 'start_recording_with_devices_and_meeting'), 'start invoke');
    expect(env.contexts.map((c: any) => [c.options, c.closed])).toEqual([
      [{ sampleRate: 48000, latencyHint: 0.5 }, true],
      [{ latencyHint: 0.5 }, false],
    ]);
    expect(env.contexts[1].sources.map((s: any) => s.input)).toEqual([0]);
  });

  test('a tab refused as a second audio source releases its microphone', async () => {
    const env = makeEnv();
    env.connect({ is_recording: true, is_paused: false }); // another tab is recording
    await until(() => env.nodes.length, 'capture');
    env.audio().onclose({ code: 4409 });
    await until(() => env.micTrack.stopped, 'capture release');
    expect(env.doc.live().map((el) => el.textContent)).toEqual([
      'Another tab is already sending audio for this recording, so this tab released its microphone.',
    ]);
    const sockets = env.audio();
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(env.audio()).toBe(sockets); // no reconnect attempts
  });

  test('re-attaches the microphone after a reload and releases it when the server stops', async () => {
    const env = makeEnv({ storage: { 'meetily.web.micDeviceId': 'usb' } });
    const ws = env.connect({ is_recording: true, is_paused: false });
    await until(() => env.nodes.length, 'capture');
    expect(env.calls).toEqual(['getUserMedia']);
    expect(env.win.navigator.mediaDevices.getUserMedia.mock.calls[0][0].audio.deviceId).toEqual({ exact: 'usb' });
    const [banner] = env.doc.live();
    expect(banner.textContent).toContain('microphone re-attached');
    expect(banner.children.map((b) => b.textContent)).toEqual(['Share tab audio', 'Dismiss']);

    ws.serverSend({ t: 'event', seq: 1, event: 'recording-stopped', payload: { message: 'stopped' } });
    await until(() => env.micTrack.stopped, 'capture release');
    expect(env.doc.live()).toEqual([]);
  });

  test('a recording-stopped while the re-attach waits on the mic prompt releases the capture', async () => {
    const env = makeEnv();
    const md = env.win.navigator.mediaDevices;
    const prompt = md.getUserMedia;
    let grant: (() => void) | undefined;
    md.getUserMedia = (c: any) => new Promise((resolve) => (grant = () => resolve(prompt(c))));
    const ws = env.connect({ is_recording: true, is_paused: false });
    await until(() => grant, 'mic prompt');
    ws.serverSend({ t: 'event', seq: 1, event: 'recording-stopped', payload: { message: 'stopped' } });
    grant!();
    await until(() => env.micTrack.stopped, 'capture release');
    expect(env.contexts[0].closed).toBe(true);
    expect(env.audio().readyState).toBe(3);
    expect(env.doc.live()).toEqual([]); // no re-attach banner
  });

  test('start replaces a re-attached capture whose recording ended unseen', async () => {
    const env = makeEnv({ devices: [{ kind: 'audioinput', deviceId: 'usb2', label: 'Other Mic' }] });
    const ws = env.connect({ is_recording: true, is_paused: false });
    await until(() => env.nodes.length, 'capture');
    const cmd = 'start_recording_with_devices_and_meeting';
    env.T.invoke(cmd, { micDeviceName: 'Other Mic (input)', systemDeviceName: null, meetingName: 'M' });
    const states = () => ws.sent.filter((m: any) => m.cmd === 'get_recording_state');
    await until(() => states().length === 2, 'state check');
    ws.serverSend({ t: 'result', id: states()[1].id, ok: true, value: { is_recording: false } });
    await until(() => env.invokeOf(ws, cmd), 'start invoke');
    expect(env.contexts[0].closed).toBe(true);
    expect(env.calls).toEqual(['getUserMedia', 'getDisplayMedia', 'getUserMedia']);
    expect(env.win.navigator.mediaDevices.getUserMedia.mock.calls[1][0].audio.deviceId).toEqual({ exact: 'usb2' });
    expect(env.nodes[1].options.processorOptions).toEqual({ mask: 3 });
    expect(env.doc.live()).toEqual([]); // the stale banner is gone
  });

  test('the audio link keeps its capture token across reconnects; a new capture gets a new one', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const a1 = await env.record(ws);
    expect(a1.url).toMatch(/^wss:\/\/meetily\.test\/api\/audio\?capture=[0-9a-f]{32}$/);
    a1.serverClose();
    const a2 = await env.reconnected(a1, '/api/audio');
    expect(a2.url).toBe(a1.url);

    a2.serverOpen();
    const stop = env.T.invoke('stop_recording', {});
    await until(() => env.invokeOf(ws, 'stop_recording'), 'stop invoke');
    env.reply(ws, 'stop_recording', { ok: true, value: null });
    await stop;
    const next = env.T.invoke('start_recording', {});
    await until(() => env.audio() !== a2, 'new capture');
    expect(env.audio().url).not.toBe(a1.url);
    void next;
  });

  test('stopping cancels a pending audio reconnect and never sends on a late open', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const audio = await env.record(ws);
    env.nodes[0].port.onmessage({ data: new ArrayBuffer(8) });
    audio.serverClose(); // drop: a reconnect is now scheduled
    ws.serverSend({ t: 'event', seq: 1, event: 'recording-stopped', payload: {} });
    await tick(30);
    expect(env.socketsAt('/api/audio')).toEqual([audio]); // the reconnect never happened

    // A socket still connecting when the capture stops is closed the moment it opens.
    const env2 = makeEnv();
    const ws2 = env2.connect();
    const start = env2.T.invoke('start_recording', {});
    await until(() => env2.invokeOf(ws2, 'start_recording'), 'start invoke');
    const late = env2.audio();
    env2.nodes[0].port.onmessage({ data: new ArrayBuffer(8) });
    ws2.serverSend({ t: 'event', seq: 1, event: 'recording-stopped', payload: {} });
    late.serverOpen();
    expect(late.sent).toEqual([]);
    expect(late.readyState).toBe(3);
    void start;
  });

  test('stop waits for buffered audio to reach the server, showing the time left', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const audio = await env.record(ws);
    audio.bufferedAmount = 3 * 50 * 1921; // 3 s of mic-only audio still in the socket buffer
    const stop = env.T.invoke('stop_recording', {});
    await until(() => env.doc.live().length, 'progress');
    expect(env.doc.live().map((el) => el.textContent)).toEqual(['Sending remaining audio… 3s left']);
    await tick(30);
    expect(env.invokeOf(ws, 'stop_recording')).toBeUndefined(); // still draining
    expect(env.micTrack.stopped).toBe(true); // the microphone is released right away
    expect(env.windowListeners.beforeunload).toHaveLength(1); // leaving now would lose audio

    audio.bufferedAmount = 0;
    await until(() => env.invokeOf(ws, 'stop_recording'), 'stop invoke');
    expect(audio.readyState).toBe(3);
    expect(env.doc.live()).toEqual([]);
    expect(env.windowListeners.beforeunload).toHaveLength(0);
    env.reply(ws, 'stop_recording', { ok: true, value: null });
    expect(await stop).toBeNull();
  });

  test('stop gives up after two minutes and says how much audio was not delivered', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const audio = await env.record(ws);
    audio.bufferedAmount = 10 * 50 * 1921;
    env.T.invoke('stop_recording', {});
    await until(() => env.doc.live().some((el) => el.textContent.startsWith('Sending')), 'progress');
    setSystemTime(new Date(Date.now() + 121_000));
    try {
      await until(() => env.invokeOf(ws, 'stop_recording'), 'stop invoke');
    } finally {
      setSystemTime();
    }
    expect(env.doc.created.map((el) => el.textContent)).toContain(
      'The last 10 seconds of audio could not be delivered to the server.'
    ); // created, not live(): notices time out after 5 ms here
    expect(env.doc.live().some((el) => el.textContent.startsWith('Sending'))).toBe(false);
  });

  test('a backlog over 5 s of audio warns once, again only after it fell below 1 s', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const audio = await env.record(ws);
    const frame = () => env.nodes[0].port.onmessage({ data: new ArrayBuffer(1921) });
    const slow = 'Your connection is slower than the audio stream. Keep this tab open until the recording catches up.';
    const shown = () => env.doc.live().map((el) => el.textContent);

    audio.bufferedAmount = 4 * 50 * 1921;
    frame();
    expect(shown()).toEqual([]);
    audio.bufferedAmount = 6 * 50 * 1921;
    frame();
    expect(shown()).toEqual([slow]);
    await until(() => !shown().length, 'notice timeout');
    frame();
    expect(shown()).toEqual([]); // still behind, but already warned
    audio.bufferedAmount = 0;
    frame();
    audio.bufferedAmount = 6 * 50 * 1921;
    frame();
    expect(shown()).toEqual([slow]);
  });
});

describe('protocol v2', () => {
  test('reconnects pass boot and since; a new boot resets the sequence', async () => {
    const env = makeEnv();
    const ws1 = env.connect(undefined, 'b/1', 7);
    expect(ws1.url).toBe('wss://meetily.test/api/ws');
    const got: any[] = [];
    await env.listen('transcript-update', (e) => got.push(e.payload));
    ws1.serverSend({ t: 'event', seq: 8, event: 'transcript-update', payload: 8 });

    ws1.serverClose();
    const ws2 = await env.reconnected(ws1);
    expect(ws2.url).toBe('wss://meetily.test/api/ws?boot=b%2F1&since=8');
    ws2.serverOpen();
    ws2.hello('b/1', 9); // same server: it replays 9 next
    ws2.serverSend({ t: 'event', seq: 9, event: 'transcript-update', payload: 9 });

    ws2.serverClose();
    const ws3 = await env.reconnected(ws2);
    expect(ws3.url).toBe('wss://meetily.test/api/ws?boot=b%2F1&since=9');
    ws3.serverOpen();
    ws3.hello('b2', 3); // the server restarted: its counter starts over
    ws3.serverSend({ t: 'event', seq: 4, event: 'transcript-update', payload: 'new' });
    expect(got).toEqual([8, 9, 'new']);

    ws3.serverClose();
    expect((await env.reconnected(ws3)).url).toBe('wss://meetily.test/api/ws?boot=b2&since=4');
  });

  test('a socket that never got a hello reconnects as a first connect', async () => {
    const env = makeEnv();
    const ws1 = env.control();
    ws1.serverClose();
    expect((await env.reconnected(ws1)).url).toBe('wss://meetily.test/api/ws');
  });

  test('replayed events already seen are dropped', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const got: any[] = [];
    await env.listen('transcript-update', (e) => got.push(e.payload));
    for (const seq of [1, 2, 2, 1, 3]) ws.serverSend({ t: 'event', seq, event: 'transcript-update', payload: seq });
    expect(got).toEqual([1, 2, 3]);
  });

  test('gap shows a notice and does not reload', () => {
    const env = makeEnv();
    const ws = env.connect();
    ws.serverSend({ t: 'gap' });
    expect(env.doc.live().map((el) => el.textContent)).toEqual([
      'Some live updates were missed while reconnecting. Reload the page if the transcript looks incomplete.',
    ]);
    expect(env.win.location.reload).not.toHaveBeenCalled();
  });

  test('unknown re-sends the original invoke', async () => {
    const env = makeEnv();
    const ws1 = env.connect();
    const call = env.T.invoke('api_get_meeting', { meetingId: 'm1' });
    const original = env.invokeOf(ws1, 'api_get_meeting');
    ws1.serverClose();
    const ws2 = await env.reconnected(ws1);
    ws2.serverOpen();
    ws2.hello('other-boot', 0);
    expect(ws2.sent).toContainEqual({ t: 'resume', ids: [original.id] });
    ws2.serverSend({ t: 'unknown', id: original.id });
    expect(ws2.sent.at(-1)).toEqual(original);
    ws2.serverSend({ t: 'result', id: original.id, ok: true, value: { id: 'm1' } });
    expect(await call).toEqual({ id: 'm1' });
  });
});

describe('files', () => {
  const dropEvent = (files: any[]) => ({
    dataTransfer: { types: ['Files'], files },
    preventDefault: mock(() => {}),
    stopPropagation: mock(() => {}),
  });

  test('dropping an audio file uploads it and emits tauri://drag-drop with the server path', async () => {
    const env = makeEnv();
    env.connect();
    const events: any[] = [];
    for (const name of ['tauri://drag-enter', 'tauri://drag-leave', 'tauri://drag-drop']) {
      await env.listen(name, (e) => events.push([e.event, e.payload]));
    }
    const file = { name: 'team sync.mp3' };
    fire(env.doc.listeners, 'dragenter', dropEvent([]));
    const drop = dropEvent([{ name: 'notes.txt' }, file]);
    fire(env.doc.listeners, 'drop', drop);
    expect(drop.preventDefault).toHaveBeenCalled();

    const [xhr] = env.xhrs;
    expect([xhr.method, xhr.url, xhr.body]).toEqual(['PUT', '/api/upload?name=team%20sync.mp3', file]);
    expect(env.doc.live()[0].textContent).toBe('Uploading team sync.mp3 0%');
    xhr.respond(200, { path: '/data/uploads/team sync.mp3' });
    await until(() => events.length === 3, 'drop event');
    expect(events).toEqual([
      ['tauri://drag-enter', { paths: [] }],
      ['tauri://drag-leave', null],
      ['tauri://drag-drop', { paths: ['/data/uploads/team sync.mp3'] }],
    ]);
    expect(env.doc.live()).toEqual([]);
  });

  test('non-audio drops and a disabled beta feature pass bare names without uploading', async () => {
    const env = makeEnv();
    env.connect();
    const drops: any[] = [];
    await env.listen('tauri://drag-drop', (e) => drops.push(e.payload.paths));
    fire(env.doc.listeners, 'drop', dropEvent([{ name: 'notes.txt' }]));
    env.storage.set('betaFeatures', JSON.stringify({ importAndRetranscribe: false }));
    fire(env.doc.listeners, 'drop', dropEvent([{ name: 'a.wav' }]));
    await until(() => drops.length === 2, 'drop events');
    expect(drops).toEqual([['notes.txt'], ['a.wav']]);
    expect(env.xhrs).toEqual([]);
  });

  test('a drop nobody listens for is ignored without uploading', () => {
    const env = makeEnv();
    env.connect();
    const drop = dropEvent([{ name: 'a.wav' }]);
    fire(env.doc.listeners, 'drop', drop);
    expect(drop.preventDefault).toHaveBeenCalled();
    expect(env.xhrs).toEqual([]);
    expect(env.doc.live()).toEqual([]);
  });

  test('select_and_validate_audio_command: pick, upload, validate on the server; cancel gives null', async () => {
    const env = makeEnv();
    const ws = env.connect();
    const file = { name: 'a.wav' };
    env.doc.onClick = (input) => {
      input.files = [file];
      input.fire('change');
    };
    const picked = env.T.invoke('select_and_validate_audio_command');
    await until(() => env.xhrs.length, 'upload');
    env.xhrs[0].respond(200, { path: '/data/uploads/a.wav' });
    await until(() => env.invokeOf(ws, 'validate_audio_file_command'), 'validate invoke');
    expect(env.invokeOf(ws, 'validate_audio_file_command').args).toEqual({ path: '/data/uploads/a.wav' });
    const info = { path: '/data/uploads/a.wav', filename: 'a.wav', duration_seconds: 1, size_bytes: 2, format: 'wav' };
    env.reply(ws, 'validate_audio_file_command', { ok: true, value: info });
    expect(await picked).toEqual(info);

    env.doc.onClick = (input) => input.fire('cancel');
    expect(await env.T.invoke('select_and_validate_audio_command')).toBeNull();
  });

  test('a failed upload rejects with the server error string', async () => {
    const env = makeEnv();
    env.connect();
    env.doc.onClick = (input) => {
      input.files = [{ name: 'a.wav' }];
      input.fire('change');
    };
    const picked = env.T.invoke('select_and_validate_audio_command');
    await until(() => env.xhrs.length, 'upload');
    env.xhrs[0].respond(413, { error: 'File too large' });
    expect(await picked.catch((e: unknown) => e)).toBe('File too large');
  });
});

describe('audio worklet', () => {
  function loadWorklet(registerProcessor?: (name: string, cls: any) => void, base?: any, sampleRate = 48000) {
    const module: any = { exports: {} };
    new Function('module', 'registerProcessor', 'AudioWorkletProcessor', 'sampleRate', WORKLET)(
      module,
      registerProcessor,
      base,
      sampleRate
    );
    return module.exports;
  }

  class FakeProcessorBase {
    port: any = {
      messages: [] as any[],
      onmessage: null,
      postMessage(msg: any) {
        this.messages.push(msg);
      },
    };
  }

  test('resample streams linearly interpolated 48 kHz samples across calls', () => {
    const { resample } = loadWorklet();
    const state = { t: 0, prev: [0] };
    // 24 kHz -> 48 kHz: step 0.5, two outputs per input sample, continuous across quanta.
    expect(resample(state, [Float32Array.from([1, 1])], 0.5)).toEqual([[0, 0.5, 1, 1]]);
    expect(resample(state, [Float32Array.from([0, 0])], 0.5)).toEqual([[1, 0.5, 0, 0]]);
    // 44.1 kHz: 441 inputs become 480 outputs.
    const s = { t: 0, prev: [0] };
    let produced = 0;
    for (let i = 0; i < 100; i++) produced += resample(s, [new Float32Array(441)], 44100 / 48000)[0].length;
    expect(Math.abs(produced - 48000)).toBeLessThanOrEqual(1);
  });

  test('a 44.1 kHz context still yields 48 kHz frames', () => {
    let Processor: any;
    loadWorklet((_name, cls) => (Processor = cls), FakeProcessorBase, 44100);
    const processor = new Processor({ processorOptions: { mask: 1 } });
    for (let i = 0; i < 345; i++) processor.process([[new Float32Array(128).fill(0.5)], []]); // ~1 s
    const frames = processor.port.messages;
    expect(frames.length).toBeGreaterThanOrEqual(49);
    expect(frames.length).toBeLessThanOrEqual(50);
    expect(new DataView(frames[1]).getInt16(1, true)).toBe(Math.round(0.5 * 32767));
  });

  test('encodeFrame writes the mask byte then clamped Int16 LE samples, sources interleaved', () => {
    const { encodeFrame } = loadWorklet();
    const mono = new Uint8Array(encodeFrame(1, [Float32Array.from([0, 0.5, -1, 2])]));
    expect([...mono]).toEqual([1, 0x00, 0x00, 0x00, 0x40, 0x01, 0x80, 0xff, 0x7f]);

    const view = new DataView(encodeFrame(3, [Float32Array.from([1, 0]), Float32Array.from([-1, 0.25])]));
    expect(view.byteLength).toBe(9);
    expect(view.getUint8(0)).toBe(3);
    expect([1, 3, 5, 7].map((offset) => view.getInt16(offset, true))).toEqual([32767, -32767, 0, 8192]);
  });

  test('the processor posts 960-sample frames, downmixes, and flushes the partial frame', () => {
    let Processor: any;
    loadWorklet((name, cls) => {
      expect(name).toBe('meetily-capture');
      Processor = cls;
    }, FakeProcessorBase);
    const processor = new Processor({ processorOptions: { mask: 3 } });
    const quantum = (value: number) => new Float32Array(128).fill(value);

    // Stereo mic (averaged) and a tab input with no channels (silence): 8 quanta = 1024 samples.
    for (let i = 0; i < 8; i++) expect(processor.process([[quantum(0.5), quantum(0.25)], []])).toBe(true);
    const { messages } = processor.port;
    expect(messages).toHaveLength(1);
    const frame = new DataView(messages[0]);
    expect(frame.byteLength).toBe(1 + 960 * 2 * 2);
    expect([frame.getUint8(0), frame.getInt16(1, true), frame.getInt16(3, true)]).toEqual([3, Math.round(0.375 * 32767), 0]);

    processor.port.onmessage({ data: 'flush' });
    expect(messages).toHaveLength(3);
    expect(messages[1].byteLength).toBe(1 + 64 * 2 * 2);
    expect(messages[2]).toBe('flushed');
  });
});
