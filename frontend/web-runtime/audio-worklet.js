// AudioWorkletProcessor 'meetily-capture', served at /__meetily/audio-worklet.js.
// Node input 0 = microphone, input 1 = tab audio. Every input selected by the source mask is downmixed
// to mono, resampled to 48 kHz if the context runs at another rate, collected into 20 ms frames and
// posted to the main thread as an ArrayBuffer:
//   byte 0 = source mask (bit0 mic, bit1 tab), then Int16 LE PCM at 48 kHz, sources interleaved.

const TARGET_RATE = 48000;
const FRAME_SAMPLES = 960; // 20 ms at 48 kHz

// One frame: per sample index, one Int16 value per source (mask order: mic first).
function encodeFrame(mask, sources) {
  const len = sources[0].length;
  const view = new DataView(new ArrayBuffer(1 + 2 * len * sources.length));
  view.setUint8(0, mask);
  let offset = 1;
  for (let i = 0; i < len; i++) {
    for (const src of sources) {
      view.setInt16(offset, Math.round(Math.max(-1, Math.min(1, src[i])) * 32767), true);
      offset += 2;
    }
  }
  return view.buffer;
}

// Averages an input's channels into dst; an input with no channels (not connected yet, or the shared
// tab stopped) contributes silence so the frame layout never changes.
function downmix(channels, dst) {
  for (let i = 0; i < dst.length; i++) {
    let sum = 0;
    for (const ch of channels) sum += ch[i];
    dst[i] = channels.length ? sum / channels.length : 0;
  }
}

// Streaming linear resampler. `state` = { t, prev[] } carries across calls: t is the read position in
// [prev, x0 .. x(q-1)], step = input samples per output sample. Returns one array per source.
function resample(state, mono, step) {
  const q = mono[0].length;
  const out = mono.map(() => []);
  while (state.t < q) {
    const i = Math.floor(state.t);
    const f = state.t - i;
    mono.forEach((x, k) => {
      const a = i === 0 ? state.prev[k] : x[i - 1];
      out[k].push(a + (x[i] - a) * f);
    });
    state.t += step;
  }
  state.t -= q;
  mono.forEach((x, k) => {
    state.prev[k] = x[q - 1];
  });
  return out;
}

if (typeof registerProcessor === 'function') {
  registerProcessor(
    'meetily-capture',
    class extends AudioWorkletProcessor {
      constructor(options) {
        super();
        this.mask = options.processorOptions.mask;
        this.sourceInputs = [0, 1].filter((i) => this.mask & (1 << i));
        this.buffers = this.sourceInputs.map(() => new Float32Array(FRAME_SAMPLES));
        this.mono = [];
        this.filled = 0;
        this.step = sampleRate / TARGET_RATE; // `sampleRate` is the context rate (worklet global)
        this.resampler = { t: 0, prev: this.sourceInputs.map(() => 0) };
        // 'flush' (on stop): post the partial frame, then confirm so the main thread can close.
        this.port.onmessage = (e) => {
          if (e.data !== 'flush') return;
          if (this.filled) this.post(this.filled);
          this.port.postMessage('flushed');
        };
      }

      process(inputs) {
        const quantum = (inputs.find((input) => input.length) || [[{ length: 128 }]])[0].length;
        if (!this.mono.length || this.mono[0].length !== quantum) {
          this.mono = this.sourceInputs.map(() => new Float32Array(quantum));
        }
        this.sourceInputs.forEach((input, k) => downmix(inputs[input] || [], this.mono[k]));
        this.append(this.step === 1 ? this.mono : resample(this.resampler, this.mono, this.step));
        return true;
      }

      append(sources) {
        const total = sources[0].length;
        let done = 0;
        while (done < total) {
          const n = Math.min(total - done, FRAME_SAMPLES - this.filled);
          sources.forEach((src, k) => {
            for (let j = 0; j < n; j++) this.buffers[k][this.filled + j] = src[done + j];
          });
          this.filled += n;
          done += n;
          if (this.filled === FRAME_SAMPLES) this.post(FRAME_SAMPLES);
        }
      }

      post(length) {
        const frame = encodeFrame(this.mask, this.buffers.map((b) => b.subarray(0, length)));
        this.port.postMessage(frame, [frame]);
        this.filled = 0;
      }
    }
  );
} else if (typeof module === 'object') {
  module.exports = { encodeFrame, resample, FRAME_SAMPLES }; // unit tests (no AudioWorklet scope)
}
