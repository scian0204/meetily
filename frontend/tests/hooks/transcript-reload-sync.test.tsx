import { afterAll, expect, mock, test } from 'bun:test';
import { act, create } from 'react-test-renderer';
import type { Transcript, TranscriptUpdate } from '../../src/types';

// Bun shares module mocks between test files; restore application modules after this suite.
const originals = {
  recordingState: { ...await import('../../src/contexts/RecordingStateContext') },
  transcriptService: { ...await import('../../src/services/transcriptService') },
  recordingService: { ...await import('../../src/services/recordingService') },
  indexedDBService: { ...await import('../../src/services/indexedDBService') },
};
afterAll(() => {
  mock.module('../../src/contexts/RecordingStateContext', () => originals.recordingState);
  mock.module('../../src/services/transcriptService', () => originals.transcriptService);
  mock.module('../../src/services/recordingService', () => originals.recordingService);
  mock.module('../../src/services/indexedDBService', () => originals.indexedDBService);
});

let isRecording = false;
let emit: (update: TranscriptUpdate) => void = () => {};
const segment = (n: number) => ({
  id: `seg-${n}`, text: `line ${n}`, display_time: `[00:0${n}]`, sequence_id: n,
  audio_start_time: n, audio_end_time: n + 1, duration: 1, confidence: 0.9,
});
const getTranscriptHistory = mock(async () => [1, 2, 3, 4, 5].map(segment));

mock.module('../../src/contexts/RecordingStateContext', () => ({ useRecordingState: () => ({ isRecording }) }));
mock.module('../../src/services/transcriptService', () => ({ transcriptService: {
  onTranscriptUpdate: async (cb: typeof emit) => { emit = cb; return () => {}; },
  getTranscriptHistory,
} }));
mock.module('../../src/services/recordingService', () => ({ recordingService: {
  onRecordingStarted: async () => () => {},
  onRecordingStopped: async () => () => {},
  getRecordingMeetingName: async () => 'Live meeting',
} }));
mock.module('../../src/services/indexedDBService', () => ({ indexedDBService: {
  init: async () => {}, saveTranscript: async () => {},
} }));

const { TranscriptProvider, useTranscripts } = await import('../../src/contexts/TranscriptContext');

let transcripts: Transcript[] = [];
function Probe() {
  transcripts = useTranscripts().transcripts;
  return null;
}
const wait = (ms: number) => act(() => new Promise<void>(r => setTimeout(r, ms)));

test('reload during recording: history is merged with live segments that arrived first', async () => {
  const renderer = create(<TranscriptProvider><Probe /></TranscriptProvider>);
  await wait(0);

  // A live segment lands before the initial backend sync flips isRecording.
  await act(async () => emit({
    text: 'line 5', timestamp: 'now', source: 'Audio', sequence_id: 5, chunk_start_time: 5,
    is_partial: false, confidence: 0.9, audio_start_time: 5, audio_end_time: 6, duration: 1,
  } as TranscriptUpdate));
  await wait(30);
  expect(transcripts.map(t => t.sequence_id)).toEqual([5]);

  isRecording = true;
  await act(async () => renderer.update(<TranscriptProvider><Probe /></TranscriptProvider>));
  await wait(0);
  expect(transcripts.map(t => t.sequence_id)).toEqual([1, 2, 3, 4, 5]);

  // Same recording, another render: no refetch.
  await act(async () => renderer.update(<TranscriptProvider><Probe /></TranscriptProvider>));
  expect(getTranscriptHistory).toHaveBeenCalledTimes(1);

  await act(async () => renderer.unmount());
});
