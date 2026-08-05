/**
 * The web build's invoke() router: local commands must not touch the network, server
 * commands must unwrap `{ ok }`, and errors must surface verbatim.
 *
 * Run with: node --test tests/lib/
 * Uses the same transpile-and-vm approach as onboarding-summary-model.test.mjs, so no
 * test runner has to be added.
 */

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import url from 'node:url';
import vm from 'node:vm';
import ts from 'typescript';

const here = path.dirname(url.fileURLToPath(import.meta.url));
const shimPath = path.join(here, '..', '..', 'src', 'lib', 'web-shim', 'core.ts');

/** Stubs for the two sibling modules core.ts imports; neither is exercised here. */
const captureStub = {
  startCapture: async () => ({ systemAudio: false }),
  stopCapture: async () => undefined,
  pauseCapture: () => undefined,
  resumeCapture: () => undefined,
  isCapturing: () => false,
  getCaptureState: () => ({
    isRecording: false,
    isPaused: false,
    startedAt: null,
    pausedMs: 0,
    systemAudio: false,
    sessionId: null,
  }),
  listInputDevices: async () => [{ kind: 'audioinput', label: 'Fake Mic', deviceId: 'a' }],
};

function loadShim({ fetchImpl }) {
  const source = fs.readFileSync(shimPath, 'utf8');
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  });

  const emitted = [];
  const requireStub = (specifier) => {
    if (specifier === './capture') return captureStub;
    if (specifier === './event') {
      return { emitLocal: (name, payload) => emitted.push([name, payload]) };
    }
    throw new Error(`unexpected import: ${specifier}`);
  };

  const module = { exports: {} };
  const storage = new Map();
  const context = {
    module,
    exports: module.exports,
    require: requireStub,
    console,
    fetch: fetchImpl,
    process: { env: {} },
    window: {
      localStorage: {
        getItem: (k) => (storage.has(k) ? storage.get(k) : null),
        setItem: (k, v) => storage.set(k, String(v)),
        removeItem: (k) => storage.delete(k),
      },
      location: { pathname: '/', href: '/' },
      open: () => undefined,
    },
    navigator: { mediaDevices: {} },
  };
  vm.runInNewContext(outputText, context, { filename: 'core.ts' });
  return { shim: module.exports, emitted };
}

test('server commands post to /api/invoke and unwrap ok', async () => {
  const calls = [];
  const { shim } = loadShim({
    fetchImpl: async (input, init) => {
      calls.push({ input, body: JSON.parse(init.body) });
      return {
        ok: true,
        status: 200,
        json: async () => ({ ok: [{ id: 'meeting-1', title: 'Weekly Sync' }] }),
      };
    },
  });

  const meetings = await shim.invoke('api_get_meetings', {});
  assert.equal(calls.length, 1);
  assert.equal(calls[0].input, '/api/invoke/api_get_meetings');
  assert.deepEqual(meetings, [{ id: 'meeting-1', title: 'Weekly Sync' }]);
});

test('server errors are rethrown with the server message', async () => {
  const { shim } = loadShim({
    fetchImpl: async () => ({
      ok: false,
      status: 500,
      json: async () => ({ error: 'Meeting not found: meeting-9' }),
    }),
  });

  await assert.rejects(
    () => shim.invoke('api_get_meeting', { meetingId: 'meeting-9' }),
    /Meeting not found: meeting-9/,
  );
});

test('local commands never reach the network', async () => {
  let fetched = 0;
  const { shim } = loadShim({
    fetchImpl: async () => {
      fetched += 1;
      return { ok: true, status: 200, json: async () => ({ ok: null }) };
    },
  });

  assert.equal(await shim.invoke('is_recording'), false);
  assert.equal(await shim.invoke('track_meeting_started', { id: 'x' }), null);
  assert.deepEqual(await shim.invoke('get_audio_devices'), ['Fake Mic']);

  const status = await shim.invoke('get_transcription_status');
  assert.deepEqual(status, { chunks_in_queue: 0, is_processing: false, last_activity_ms: 0 });

  assert.equal(fetched, 0, 'local handlers must not call fetch');
});

test('desktop-only commands fail loudly instead of silently', async () => {
  const { shim } = loadShim({
    fetchImpl: async () => {
      throw new Error('should not be called');
    },
  });

  await assert.rejects(() => shim.invoke('select_legacy_database_path'), /not available/);
});

test('onboarding state round-trips through localStorage', async () => {
  const { shim } = loadShim({
    fetchImpl: async () => {
      throw new Error('should not be called');
    },
  });

  assert.equal(await shim.invoke('check_first_launch'), true);
  await shim.invoke('complete_onboarding', { model: 'qwen3.5:2b' });
  assert.equal(await shim.invoke('check_first_launch'), false);

  const status = await shim.invoke('get_onboarding_status');
  assert.equal(status.completed, true);
  assert.equal(status.model_status.selected_summary_model, 'qwen3.5:2b');
});
