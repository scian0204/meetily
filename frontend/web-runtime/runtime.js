// Meetily web runtime, served at /__meetily/runtime.js and injected first in every page's <head>.
// It implements window.__TAURI_INTERNALS__ (the contract every @tauri-apps/* package goes through) over
// a WebSocket to meetily-server, so the unmodified Tauri frontend runs in a plain browser. Browser-only
// work stays here: device listing, microphone/tab-audio capture streamed to /api/audio, file upload,
// drag & drop. Everything else is forwarded to the server as a Tauri command.
(function () {
  'use strict';
  if (typeof window === 'undefined' || window.__TAURI_INTERNALS__) return; // not a browser, or real Tauri

  const SAMPLE_RATE = 48000;
  const LATENCY = 0.5; // seconds of render buffering for the capture AudioContext
  const TAB_AUDIO = 'Browser Tab Audio';
  const NO_TAB_AUDIO = 'None (microphone only)';
  const AUDIO_BUSY = 4409; // close code: another tab already streams audio for this recording
  const AUDIO_EXTS = ['mp4', 'm4a', 'wav', 'mp3', 'flac', 'ogg', 'aac', 'mkv', 'webm', 'wma'];
  const MIC_KEY = 'meetily.web.micDeviceId';
  const MAX_QUEUED_FRAMES = 750; // ~15 s of 20 ms frames kept while the audio socket reconnects
  const FRAMES_PER_SECOND = 50; // worklet frames are 20 ms: 1 mask byte + 960 Int16 samples per source
  const DRAIN_MS = 120000; // how long stop waits for queued audio to reach the server
  const SLOW_SECONDS = 5; // audio backlog that shows the slow-connection notice (re-armed below 1 s)
  const doc = window.document;
  const log = (...args) => console.warn('[meetily-web]', ...args);
  const sleep = (ms) => new Promise((resolve) => window.setTimeout(resolve, ms));
  const uid = () =>
    Array.from(window.crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, '0')).join('');

  // ---- Callback registry (same contract as tauri/scripts/core.js) ----

  const callbacks = new Map();

  function transformCallback(callback, once = false) {
    const id = window.crypto.getRandomValues(new Uint32Array(1))[0];
    callbacks.set(id, (data) => {
      if (once) callbacks.delete(id);
      return callback && callback(data);
    });
    return id;
  }

  function unregisterCallback(id) {
    callbacks.delete(id);
  }

  function runCallback(id, data) {
    const callback = callbacks.get(id);
    if (callback) callback(data);
    else log(`callback ${id} not found (page reloaded during an async operation?)`);
  }

  // ---- Events: name -> Map(eventId -> handler callback id) ----

  const listeners = new Map();
  let nextEventId = 1;

  function addListener(event, handler) {
    if (!listeners.has(event)) listeners.set(event, new Map());
    const eventId = nextEventId++;
    listeners.get(event).set(eventId, handler);
    sendListen(event);
    return eventId;
  }

  // Used both by __TAURI_EVENT_PLUGIN_INTERNALS__.unregisterListener and plugin:event|unlisten.
  function removeListener(event, eventId) {
    const handlers = listeners.get(event);
    if (handlers && handlers.has(eventId)) {
      unregisterCallback(handlers.get(eventId));
      handlers.delete(eventId);
    }
    return null;
  }

  function dispatch(event, payload) {
    for (const [id, handler] of [...(listeners.get(event) || [])]) {
      try {
        runCallback(handler, { event, id, payload });
      } catch (e) {
        console.error(`[meetily-web] listener for ${event} threw`, e);
      }
    }
  }

  // tauri:// events are webview-local. Others go to the server, which echoes them to every tab
  // (this one included), so they are not dispatched here.
  function emitEvent({ event, payload }) {
    if (event.startsWith('tauri://')) dispatch(event, payload);
    else sendControl({ t: 'emit', event, payload });
    return null;
  }

  // ---- Sockets ----

  const wsUrl = (path) => `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}${path}`;

  // WebSocket that reconnects with backoff (250 ms -> 5 s) until close() is called, or until the
  // server refuses it with AUDIO_BUSY (then onbusy runs instead). path() is re-read on every connect.
  function persistentSocket(path, { binary, onopen, onmessage, onfail, onbusy }) {
    let sock;
    let timer;
    let delay = 250;
    let closed = false;
    const connect = () => {
      if (closed) return;
      let opened = false;
      const url = path();
      const s = (sock = new window.WebSocket(wsUrl(url)));
      if (binary) s.binaryType = 'arraybuffer';
      s.onopen = () => {
        if (closed) return s.close(); // opened after close(): never let it send
        opened = true;
        delay = 250;
        if (onopen) onopen();
      };
      s.onmessage = onmessage || null;
      s.onclose = (e) => {
        if (closed) return;
        if (e && e.code === AUDIO_BUSY) {
          closed = true;
          if (onbusy) onbusy();
          return;
        }
        log(`${url} closed; reconnecting in ${delay} ms`);
        if (!opened && onfail) onfail();
        timer = window.setTimeout(connect, delay);
        delay = Math.min(delay * 2, 5000);
      };
    };
    connect();
    return {
      isOpen: () => sock.readyState === 1,
      send: (data) => sock.send(data),
      buffered: () => sock.bufferedAmount,
      close: () => {
        closed = true;
        window.clearTimeout(timer);
        sock.close();
      },
    };
  }

  // ---- Control socket: invokes, results and events share one ordered stream ----
  // Every connection starts with hello {boot, seq}; events carry a seq. A reconnect passes ?boot&since
  // so the same server replays the events it missed ('gap' if it cannot), and answers 'unknown' for
  // a resumed invoke it never received.

  const pending = new Map(); // invoke id -> { resolve, reject, cmd, args }
  const outbox = []; // messages waiting for the socket; never dropped
  const listenSent = new Set(); // event names announced on the current connection
  let control = null;
  let connectedOnce = false;
  let knownBoot = null; // server instance from the last hello; null until the first one
  let lastSeq = 0; // last event sequence number dispatched (or skipped by a fresh hello)

  // Reconnects of a known server ask it to replay the events this page missed.
  const controlPath = () =>
    knownBoot === null
      ? '/api/ws'
      : `/api/ws?boot=${encodeURIComponent(knownBoot)}&since=${encodeURIComponent(lastSeq)}`;

  function sendControl(msg) {
    if (control && control.isOpen()) control.send(JSON.stringify(msg));
    else outbox.push(msg);
  }

  function sendListen(event) {
    if (event.startsWith('tauri://') || listenSent.has(event) || !control || !control.isOpen()) return;
    listenSent.add(event);
    control.send(JSON.stringify({ t: 'listen', event }));
  }

  function onControlOpen() {
    listenSent.clear();
    for (const [event, handlers] of listeners) if (handlers.size) sendListen(event);
    // Invokes sent on a previous connection are resumed; queued ones are simply sent now.
    const queued = new Set(outbox.map((msg) => msg.id));
    const ids = [...pending.keys()].filter((id) => !queued.has(id));
    if (ids.length) control.send(JSON.stringify({ t: 'resume', ids }));
    for (const msg of outbox.splice(0)) control.send(JSON.stringify(msg));
    if (!connectedOnce) {
      connectedOnce = true;
      afterLoad(reattachCapture);
    }
  }

  function onControlMessage(e) {
    let msg;
    try {
      msg = JSON.parse(e.data);
    } catch (err) {
      return log('unparseable control frame', err);
    }
    if (msg.t === 'hello') return onHello(msg);
    if (msg.t === 'event') return onEvent(msg);
    if (msg.t === 'gap') return notice(GAP_NOTICE);
    if (msg.t === 'unknown') return resend(msg.id);
    if (msg.t !== 'result') return log('unknown control frame', msg);
    const call = pending.get(msg.id);
    if (!call) return log('result for unknown invoke', msg.id);
    pending.delete(msg.id);
    if (msg.ok) call.resolve(msg.value === undefined ? null : msg.value);
    else call.reject(msg.error); // raw value: the frontend compares error strings
  }

  // First frame of every connection. A new boot (first connect or server restart) replays nothing.
  function onHello({ boot, seq }) {
    if (boot === knownBoot) return; // same server: it replays what we missed next
    knownBoot = boot;
    lastSeq = seq;
  }

  function onEvent({ seq, event, payload }) {
    if (seq <= lastSeq) return; // already seen (replay overlap)
    lastSeq = seq;
    dispatch(event, payload);
  }

  const GAP_NOTICE =
    'Some live updates were missed while reconnecting. Reload the page if the transcript looks incomplete.';

  // The server never got this invoke (lost in flight, or it restarted): send it again.
  function resend(id) {
    const call = pending.get(id);
    if (call) sendControl({ t: 'invoke', id, cmd: call.cmd, args: call.args });
    else log('unknown for an invoke that is not pending', id);
  }

  // A socket that never opened may mean the session expired: go to the login page.
  async function checkSession() {
    try {
      const res = await window.fetch('/api/session', { credentials: 'same-origin' });
      if (res.status === 401) {
        const { pathname, search } = window.location;
        window.location.href = '/login?next=' + encodeURIComponent(pathname + search);
      }
    } catch (e) {
      log('session check failed', e);
    }
  }

  // ---- invoke ----

  function forward(cmd, args) {
    return new Promise((resolve, reject) => {
      const id = uid();
      pending.set(id, { resolve, reject, cmd, args });
      sendControl({ t: 'invoke', id, cmd, args });
    });
  }

  function invoke(cmd, payload = {}) {
    const local = localHandler(cmd);
    return local ? Promise.resolve().then(() => local(payload, cmd)) : forward(cmd, payload);
  }

  function localHandler(cmd) {
    if (Object.hasOwn(LOCAL, cmd)) return LOCAL[cmd];
    if (cmd.startsWith('track_')) return nil;
    if (cmd.startsWith('plugin:updater|')) return noUpdates;
    return null;
  }

  // ---- Commands answered in the browser ----

  const nil = () => null;
  const no = () => false;
  const yes = () => true;
  const noUpdates = () => {
    throw 'Updates are not available in the web version';
  };
  const consoleOnly = (_args, cmd) => {
    console.info(`[meetily-web] ${cmd} is not available in the browser`);
    return null;
  };
  const noFolder = (_args, cmd) => {
    console.info(`[meetily-web] ${cmd}: server folders cannot be opened from the browser`);
    notice('Folders are on the server and cannot be opened from the browser.');
    return null;
  };

  const LOCAL = {
    'plugin:event|listen': ({ event, handler }) => addListener(event, handler),
    'plugin:event|unlisten': ({ event, eventId }) => removeListener(event, eventId),
    'plugin:event|emit': emitEvent,
    'plugin:event|emit_to': emitEvent,
    'plugin:process|exit': nil,
    'plugin:process|restart': () => {
      window.location.reload();
      return null;
    },
    'plugin:updater|check': nil,
    get_audio_devices: listDevices,
    trigger_microphone_permission: requestMicPermission,
    trigger_system_audio_permission_command: yes,
    check_system_audio_permissions_command: yes,
    check_screen_recording_permission_command: yes,
    request_screen_recording_permission_command: nil,
    check_homebrew_database: nil, // probes macOS desktop install paths; never on a server
    get_active_audio_output: () => ({ device_name: 'Browser', is_bluetooth: false, sample_rate: null, device_type: 'Unknown' }),
    start_audio_level_monitoring: nil,
    stop_audio_level_monitoring: nil,
    is_audio_level_monitoring: no,
    // Telemetry is off in the web version.
    init_analytics: nil,
    disable_analytics: nil,
    identify_user: nil,
    start_analytics_session: nil,
    end_analytics_session: nil,
    is_analytics_enabled: no,
    is_analytics_session_active: no,
    open_external_url: ({ url }) => {
      window.open(url, '_blank', 'noopener');
      return null;
    },
    open_meeting_folder: ({ meetingId }) => {
      window.open(`/api/meetings/${encodeURIComponent(meetingId)}/files/`, '_blank');
      return null;
    },
    open_recordings_folder: noFolder,
    open_models_folder: noFolder,
    open_parakeet_models_folder: noFolder,
    open_database_folder: noFolder,
    show_console: consoleOnly,
    hide_console: consoleOnly,
    toggle_console: consoleOnly,
    open_system_settings: consoleOnly,
    select_legacy_database_path: nil,
    select_and_validate_audio_command: pickAudioFile,
    start_recording: startRecording,
    start_recording_with_devices: startRecording,
    start_recording_with_devices_and_meeting: startRecording,
    stop_recording: stopRecording,
  };

  // ---- Devices ----

  const deviceIds = new Map(); // listed microphone name -> browser deviceId ('' = browser default)
  const media = () => window.navigator.mediaDevices;

  async function listDevices() {
    const md = media();
    let all = [];
    try {
      if (md && md.enumerateDevices) all = await md.enumerateDevices();
    } catch (e) {
      log('enumerateDevices failed', e);
    }
    deviceIds.clear();
    all
      .filter((d) => d.kind === 'audioinput' && d.deviceId !== 'default' && d.deviceId !== 'communications')
      .forEach((d, i) => {
        const base = d.label || `Microphone ${i + 1}`; // labels stay empty until mic permission
        let name = base;
        for (let n = 2; deviceIds.has(name); n++) name = `${base} #${n}`;
        deviceIds.set(name, d.deviceId);
      });
    if (!deviceIds.size) deviceIds.set('Microphone', ''); // no Input device hides the record button
    const devices = [...deviceIds.keys()].map((name) => ({ name, device_type: 'Input' }));
    // "Output" = what plays the other meeting participants. The app's "Default" choice behaves like
    // TAB_AUDIO (the desktop app records the default output); NO_TAB_AUDIO opts out.
    if (md && md.getDisplayMedia) devices.push({ name: TAB_AUDIO, device_type: 'Output' });
    devices.push({ name: NO_TAB_AUDIO, device_type: 'Output' });
    return devices;
  }

  const wantsTabAudio = (systemDeviceName) =>
    Boolean(media() && media().getDisplayMedia) &&
    (systemDeviceName || '').replace(/ \((input|output)\)$/, '') !== NO_TAB_AUDIO;

  // The frontend passes "<name> (input)", or null for the default device.
  async function micDeviceId(name) {
    if (!name) return undefined;
    const label = name.replace(/ \((input|output)\)$/, '');
    if (!deviceIds.has(label)) await listDevices();
    return deviceIds.get(label) || undefined;
  }

  async function requestMicPermission() {
    try {
      (await getMic()).getTracks().forEach((t) => t.stop());
      return true;
    } catch (e) {
      log('microphone permission not granted', e);
      return false;
    }
  }

  // ---- Capture: microphone (+ tab audio) -> AudioWorklet -> /api/audio ----

  let capture = null; // { ctx, node, link, tracks, micId, queue, onFlushed, sources, slow }
  let stopGen = 0; // bumped by every recording-stopped; a capture started across a bump is released

  async function getMic(deviceId) {
    const md = media();
    if (!md || !md.getUserMedia) throw new Error('browser audio capture requires HTTPS or localhost');
    const constraints = (id) => ({
      audio: {
        deviceId: id ? { exact: id } : undefined,
        channelCount: 1,
        echoCancellation: true,
        noiseSuppression: false, // the server applies its own filtering and loudness normalisation
        autoGainControl: false,
      },
    });
    try {
      return await md.getUserMedia(constraints(deviceId));
    } catch (e) {
      if (!deviceId || e.name !== 'OverconstrainedError') throw e;
      log('selected microphone is gone, using the default one', e);
      return md.getUserMedia(constraints());
    }
  }

  // Asks the user to share a tab; returns its audio track, or null (recording continues mic-only).
  async function getTabAudio() {
    try {
      const stream = await media().getDisplayMedia({
        video: true,
        audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false },
        systemAudio: 'include',
        selfBrowserSurface: 'exclude',
      });
      stream.getVideoTracks().forEach((t) => t.stop());
      const [track] = stream.getAudioTracks();
      if (track) return track;
      notice('The shared tab has no audio ("Share tab audio" was off). Recording the microphone only.');
    } catch (e) {
      log('tab audio not shared', e);
      notice('Tab audio was not shared. Recording the microphone only.');
    }
    return null;
  }

  async function startCapture(micId, tabTrack) {
    let mic;
    try {
      mic = await getMic(micId);
    } catch (e) {
      if (tabTrack) tabTrack.stop();
      throw `microphone unavailable: ${(e && e.message) || e}`; // the UI classifies errors by substring
    }
    const tracks = [...mic.getTracks(), ...(tabTrack ? [tabTrack] : [])];
    let ctx;
    try {
      let micSource;
      ({ ctx, micSource } = await openContext(mic));
      await ctx.audioWorklet.addModule('/__meetily/audio-worklet.js');
      const node = new window.AudioWorkletNode(ctx, 'meetily-capture', {
        numberOfInputs: 2,
        numberOfOutputs: 1,
        outputChannelCount: [1],
        processorOptions: { mask: tabTrack ? 3 : 1 },
      });
      micSource.connect(node, 0, 0);
      if (tabTrack) ctx.createMediaStreamSource(new window.MediaStream([tabTrack])).connect(node, 0, 1);
      const mute = ctx.createGain();
      mute.gain.value = 0; // the node must reach the destination to be pulled, but stays inaudible
      node.connect(mute).connect(ctx.destination);

      const micId = mic.getAudioTracks()[0].getSettings().deviceId;
      const c = { ctx, node, tracks, micId, queue: [], onFlushed: null, sources: tabTrack ? 2 : 1, slow: false };
      const token = uid(); // the server lets this capture's reconnects take over its own slot
      c.link = persistentSocket(() => `/api/audio?capture=${token}`, {
        binary: true,
        onopen: () => sendFrames(c),
        onbusy: () => {
          notice('Another tab is already sending audio for this recording, so this tab released its microphone.');
          if (capture === c) stopCapture(false);
        },
      });
      node.port.onmessage = (e) => {
        if (e.data === 'flushed') return c.onFlushed && c.onFlushed();
        c.queue.push(e.data);
        if (c.queue.length > MAX_QUEUED_FRAMES) {
          c.queue.shift();
          if (!c.dropping) log('audio socket backlog full; dropping the oldest audio');
          c.dropping = true;
        }
        sendFrames(c);
        watchBacklog(c);
      };
      if (ctx.state !== 'running') resumeOnGesture(ctx);
      capture = c;
      if (c.micId) writeStore(MIC_KEY, c.micId);
      window.addEventListener('beforeunload', confirmLeave);
      return c;
    } catch (e) {
      tracks.forEach((t) => t.stop());
      if (ctx) ctx.close().catch((err) => log('closing audio context failed', err));
      throw `audio capture failed: ${(e && e.message) || e}`;
    }
  }

  // 48 kHz avoids resampling anywhere. Firefox cannot connect a stream whose rate differs from the
  // context's; then the context runs at the device rate and the worklet resamples to 48 kHz.
  // Capture has no audible output, so large render buffers (LATENCY) cost nothing; small default
  // buffers underrun on a busy machine or in a background window, and every underrun loses audio.
  async function openContext(mic) {
    const ctx = new window.AudioContext({ sampleRate: SAMPLE_RATE, latencyHint: LATENCY });
    try {
      return { ctx, micSource: ctx.createMediaStreamSource(mic) };
    } catch (e) {
      await ctx.close();
      if (e.name !== 'NotSupportedError') throw e;
      log('microphone rate is not 48 kHz; resampling in the worklet', e);
      const native = new window.AudioContext({ latencyHint: LATENCY });
      return { ctx: native, micSource: native.createMediaStreamSource(mic) };
    }
  }

  function sendFrames(c) {
    while (c.queue.length && c.link.isOpen()) c.link.send(c.queue.shift());
  }

  const byteRate = (c) => FRAMES_PER_SECOND * (1 + 1920 * c.sources); // audio bytes per second
  // Bytes not yet handed to the network: queued frames plus the socket's own send buffer.
  const backlog = (c) => c.queue.reduce((n, frame) => n + frame.byteLength, 0) + c.link.buffered();

  function watchBacklog(c) {
    const seconds = backlog(c) / byteRate(c);
    if (seconds < 1) c.slow = false;
    if (seconds <= SLOW_SECONDS || c.slow) return;
    c.slow = true;
    notice('Your connection is slower than the audio stream. Keep this tab open until the recording catches up.');
  }

  // Autoplay policy keeps a context created without a recent gesture suspended until the next one.
  function resumeOnGesture(ctx) {
    const resume = () => {
      if (ctx.state === 'suspended') ctx.resume().catch((e) => log('could not resume audio', e));
    };
    resume();
    doc.addEventListener('pointerdown', resume, { once: true, capture: true });
  }

  // flush: post the worklet's partial frame, release the devices, then send the backlog (drain).
  async function stopCapture(flush) {
    const c = capture;
    if (!c) return;
    capture = null;
    removeUi('banner');
    if (flush) {
      await new Promise((resolve) => {
        c.onFlushed = resolve;
        c.node.port.postMessage('flush');
        window.setTimeout(resolve, 500); // a suspended context never answers
      });
    }
    c.tracks.forEach((t) => t.stop());
    c.ctx.close().catch((e) => log('closing audio context failed', e));
    if (flush) await drain(c);
    c.link.close();
    if (!capture) window.removeEventListener('beforeunload', confirmLeave); // kept while draining
  }

  // Waits (<= DRAIN_MS) until every queued byte has left the browser; reports what could not.
  async function drain(c) {
    const start = Date.now();
    const initial = backlog(c);
    let left = initial;
    while (left > 0 && Date.now() - start < DRAIN_MS) {
      const elapsed = (Date.now() - start) / 1000;
      const rate = elapsed >= 1 && left < initial ? (initial - left) / elapsed : byteRate(c); // measured, else nominal
      showUi('sending', `Sending remaining audio… ${Math.ceil(left / rate)}s left`, 'bottom:16px;right:16px');
      await sleep(100);
      left = backlog(c);
    }
    removeUi('sending');
    if (left <= 0) return;
    const seconds = Math.ceil(left / byteRate(c));
    log(`stopping with ${left} audio bytes unsent`);
    notice(`The last ${seconds} seconds of audio could not be delivered to the server.`);
  }

  function confirmLeave(e) {
    e.preventDefault();
    e.returnValue = '';
  }

  // Capture starts before the server command so permission prompts never run inside it.
  async function startRecording(args, cmd) {
    if (capture) {
      // A re-attached capture serves the server's recording (the server decides). One whose recording
      // ended unnoticed is replaced; this round trip stays well inside the click's user activation.
      const state = await forward('get_recording_state', {});
      if (state && state.is_recording) return forward(cmd, args);
      await stopCapture(false);
    }
    // getDisplayMedia must be the first slow await: it needs the click's short-lived user activation.
    const tab = wantsTabAudio(args.systemDeviceName) ? await getTabAudio() : null;
    await startCapture(await micDeviceId(args.micDeviceName), tab);
    try {
      return await forward(cmd, args);
    } catch (e) {
      await stopCapture(false);
      throw e;
    }
  }

  async function stopRecording(args) {
    await stopCapture(true);
    return forward('stop_recording', args);
  }

  // After a reload during a recording the server is still recording but this tab's streams are gone.
  async function reattachCapture() {
    try {
      const state = await forward('get_recording_state', {});
      if (!state || !state.is_recording || capture) return;
      const gen = stopGen;
      const c = await startCapture(readStore(MIC_KEY) || undefined, null);
      if (gen !== stopGen) return stopCapture(false); // stopped while the mic prompt was open
      showReattachBanner(c);
    } catch (e) {
      log('could not re-attach the microphone', e);
      notice(`A recording is in progress, but the microphone could not be re-attached: ${e}`);
    }
  }

  function showReattachBanner(c) {
    let text = 'Recording in progress — microphone re-attached';
    if (c.ctx.state !== 'running') text += ' (click anywhere to resume audio)';
    const banner = showUi('banner', text, 'top:16px;left:50%;transform:translateX(-50%)');
    if (media().getDisplayMedia) banner.append(button('Share tab audio', () => shareTabAudio(c.micId)));
    banner.append(button('Dismiss', () => removeUi('banner')));
  }

  async function shareTabAudio(micId) {
    const gen = stopGen;
    const tab = await getTabAudio(); // first await: needs this click's user activation
    if (!tab) return;
    await stopCapture(true);
    try {
      await startCapture(micId, tab);
      if (gen !== stopGen) await stopCapture(false); // the recording ended meanwhile
    } catch (e) {
      log('could not restart capture with tab audio', e);
      notice(`Audio capture stopped: ${e}`);
    }
  }

  // ---- Files: upload, picker, drag & drop ----

  // PUT the raw file; resolves with its server path, rejects with a string.
  function upload(file) {
    const progress = (pct) => showUi('progress', `Uploading ${file.name} ${pct}%`, 'bottom:16px;right:16px');
    progress(0);
    return new Promise((resolve, reject) => {
      const xhr = new window.XMLHttpRequest();
      xhr.open('PUT', '/api/upload?name=' + encodeURIComponent(file.name));
      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable) progress(Math.round((e.loaded / e.total) * 100));
      };
      xhr.onload = () => {
        let body = {};
        try {
          body = JSON.parse(xhr.responseText);
        } catch (e) {
          log('upload response is not JSON', e);
        }
        if (xhr.status === 200 && body.path) resolve(body.path);
        else reject(body.error || `Upload failed (HTTP ${xhr.status})`);
      };
      xhr.onerror = () => reject('Upload failed: network error');
      xhr.send(file);
    }).finally(() => removeUi('progress'));
  }

  // select_and_validate_audio_command: file picker -> upload -> server-side validation (AudioFileInfo).
  async function pickAudioFile() {
    const input = doc.createElement('input');
    input.type = 'file';
    input.accept = AUDIO_EXTS.map((ext) => '.' + ext).join(',');
    input.style.display = 'none';
    (doc.body || doc.documentElement).appendChild(input);
    const file = await new Promise((resolve) => {
      input.addEventListener('change', () => resolve(input.files[0] || null));
      input.addEventListener('cancel', () => resolve(null));
      input.click();
    }).finally(() => input.remove());
    if (!file) return null;
    return forward('validate_audio_file_command', { path: await upload(file) });
  }

  const isAudio = (name) => AUDIO_EXTS.includes(name.split('.').pop().toLowerCase());

  function importDisabled() {
    try {
      return JSON.parse(readStore('betaFeatures') || '{}').importAndRetranscribe === false;
    } catch (e) {
      log('unreadable betaFeatures', e);
      return false;
    }
  }

  // Emulates the webview's tauri://drag-* events. The desktop webview never shows file drags to the
  // page, so file drag events are consumed here too.
  function installDragDrop() {
    let depth = 0; // dragenter/dragleave nesting; back to 0 = the pointer left the window
    const on = (type, fn) =>
      doc.addEventListener(
        type,
        (e) => {
          if (!e.dataTransfer || !Array.from(e.dataTransfer.types).includes('Files')) return;
          e.preventDefault();
          e.stopPropagation();
          fn(e);
        },
        true
      );
    on('dragenter', () => {
      if (depth++ === 0) dispatch('tauri://drag-enter', { paths: [] });
    });
    on('dragover', () => {});
    on('dragleave', () => {
      if (depth > 0 && --depth === 0) dispatch('tauri://drag-leave', null);
    });
    on('drop', (e) => {
      depth = 0;
      // Like the webview: a drop nobody listens for is ignored (and must not upload anything).
      if ((listeners.get('tauri://drag-drop') || new Map()).size) dropFiles(Array.from(e.dataTransfer.files));
    });
  }

  async function dropFiles(files) {
    const audio = files.find((f) => isAudio(f.name));
    // The app shows its own error toast for these, so pass the bare names without uploading.
    if (!audio || importDisabled()) return dispatch('tauri://drag-drop', { paths: files.map((f) => f.name) });
    dispatch('tauri://drag-leave', null); // hide the drop overlay while uploading
    try {
      dispatch('tauri://drag-drop', { paths: [await upload(audio)] });
    } catch (e) {
      log('dropped file upload failed', e);
      notice(`Upload failed: ${e}`);
    }
  }

  // ---- Small helpers: storage, overlay UI ----

  function readStore(key) {
    try {
      return window.localStorage.getItem(key);
    } catch (e) {
      log('localStorage unavailable', e);
      return null;
    }
  }

  function writeStore(key, value) {
    try {
      window.localStorage.setItem(key, value);
    } catch (e) {
      log('localStorage unavailable', e);
    }
  }

  // Overlay DOM is only inserted after load, so it never races React's hydration of <body>.
  function afterLoad(fn) {
    if (doc.readyState === 'complete') fn();
    else window.addEventListener('load', fn, { once: true });
  }

  const ui = {}; // live overlay elements by key
  const OVERLAY =
    'position:fixed;z-index:2147483647;display:flex;gap:8px;align-items:center;max-width:90vw;' +
    'padding:8px 14px;border-radius:8px;background:#1f2937;color:#fff;' +
    'font:13px/1.4 system-ui,sans-serif;box-shadow:0 4px 12px rgba(0,0,0,.25);';

  function showUi(key, text, position) {
    if (!ui[key]) {
      ui[key] = doc.createElement('div');
      ui[key].setAttribute('role', 'status');
      ui[key].style.cssText = OVERLAY + position;
      (doc.body || doc.documentElement).appendChild(ui[key]);
    }
    ui[key].textContent = text;
    return ui[key];
  }

  function removeUi(key) {
    if (ui[key]) ui[key].remove();
    delete ui[key];
  }

  let noticeTimer;
  function notice(text) {
    showUi('notice', text, 'bottom:16px;left:50%;transform:translateX(-50%)');
    window.clearTimeout(noticeTimer);
    noticeTimer = window.setTimeout(() => removeUi('notice'), 6000);
  }

  function button(label, onClick) {
    const b = doc.createElement('button');
    b.type = 'button';
    b.textContent = label;
    b.style.cssText = 'background:#fff;color:#1f2937;border:0;border-radius:6px;padding:2px 10px;cursor:pointer;font:inherit';
    b.addEventListener('click', onClick);
    return b;
  }

  // ---- Install ----

  window.__TAURI_INTERNALS__ = {
    invoke,
    transformCallback,
    unregisterCallback,
    runCallback,
    callbacks,
    convertFileSrc: (path) => path,
    metadata: { currentWindow: { label: 'main' }, currentWebview: { windowLabel: 'main', label: 'main' } },
    plugins: { path: { sep: '/', delimiter: ':' } },
  };
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: removeListener };
  // 'linux' skips the macOS-only permission onboarding step and permission warnings.
  window.__TAURI_OS_PLUGIN_INTERNALS__ = {
    platform: 'linux',
    family: 'unix',
    os_type: 'linux',
    arch: 'x86_64',
    version: '',
    eol: '\n',
    exe_extension: '',
  };

  // The server ended the recording (stopped from another tab, or an error): release the microphone.
  addListener(
    'recording-stopped',
    transformCallback(() => {
      stopGen++;
      stopCapture(false);
    })
  );
  installDragDrop();
  control = persistentSocket(controlPath, { onopen: onControlOpen, onmessage: onControlMessage, onfail: checkSession });
})();
