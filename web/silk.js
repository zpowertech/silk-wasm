/**
 * silk.js — decode SILK v3 (`.slk` / `.silk`) audio with a Rust-compiled
 * WebAssembly module, and play it with the Web Audio API.
 *
 * The WASM module is dependency-free: it is a plain `.wasm` file with a small
 * C-style ABI, so this wrapper is all the glue required. It works in browsers and
 * in Node 18+ (which is how the bundled test suite exercises it).
 *
 * Typical use:
 *
 * ```js
 * import { loadSilk, playSilk } from './silk.js';
 *
 * const silk = await loadSilk();               // loads ./silk_core.wasm
 * const bytes = new Uint8Array(await (await fetch(url)).arrayBuffer());
 * const audio = silk.decode(bytes);            // { samples, sampleRate, duration }
 * await playSilk(audio);                       // browser only
 * ```
 */

/** Default location of the wasm payload, relative to this module. */
const DEFAULT_WASM_URL = new URL('./silk_core.wasm', import.meta.url);

/** Out-buffer header layout (see crates/silk-core-wasm/src/lib.rs). */
const HEADER_WORDS = 4;

/**
 * Convert 16-bit PCM to the `[-1, 1)` float range Web Audio works in.
 *
 * @param {Int16Array} samples
 * @returns {Float32Array}
 */
export function float32From(samples) {
  const out = new Float32Array(samples.length);
  for (let i = 0; i < samples.length; i++) out[i] = samples[i] / 32768;
  return out;
}

/**
 * An error raised by the decoder. Carries the message produced by the Rust core.
 */
export class SilkError extends Error {
  constructor(message) {
    super(message);
    this.name = 'SilkError';
  }
}

/**
 * A loaded SILK decoder.
 *
 * Instances are created by {@link loadSilk}. They hold the WASM instance and
 * manage scratch memory, so reuse one instance for as many files as you like.
 */
export class Silk {
  /** @param {WebAssembly.Instance} instance */
  constructor(instance) {
    this.instance = instance;
    this.exports = instance.exports;
    /** Cache of the last decode, so `samples` can be a subarray view. */
    this._scratch = null;
  }

  /** Absolute URL this module was loaded from (browser) — informational. */
  static get defaultWasmUrl() {
    return DEFAULT_WASM_URL;
  }

  /**
   * True when `bytes` starts with a recognised SILK header.
   * @param {Uint8Array} bytes
   * @returns {boolean}
   */
  isSilk(bytes) {
    return this._withBytes(bytes, (ptr, len) => this.exports.silk_is_silk(ptr, len) === 1);
  }

  /**
   * Native sample rate of a SILK stream in Hz.
   *
   * Decoding at this rate needs no resampling and is bit-accurate against the
   * SILK reference decoder.
   *
   * @param {Uint8Array} bytes
   * @returns {number}
   */
  detectSampleRate(bytes) {
    return this._withBytes(bytes, (ptr, len) => {
      const rate = this.exports.silk_detect_sample_rate(ptr, len);
      if (rate === 0) throw this._lastError();
      return rate;
    });
  }

  /**
   * Split a SILK container into packets.
   *
   * Returns views into a copy of `bytes`, so they stay valid after this call.
   *
   * @param {Uint8Array} bytes
   * @returns {Uint8Array[]}
   */
  packets(bytes) {
    return this._withBytes(bytes, (ptr, len) => {
      const count = this.exports.silk_packet_count(ptr, len);
      const out = [];
      let offset = this._headerLength(bytes);
      for (let i = 0; i < count; i++) {
        const n = this.exports.silk_packet_len(ptr, len, i);
        out.push(bytes.subarray(offset + 2, offset + 2 + n));
        offset += 2 + n;
      }
      return out;
    });
  }

  /**
   * Decode a complete SILK file held in memory.
   *
   * @param {Uint8Array} bytes raw file bytes, including the `#!SILK_V3` header
   * @returns {{samples: Int16Array, sampleRate: number, duration: number}}
   */
  decode(bytes) {
    return this._withBytes(bytes, (ptr, len) => {
      const ratePtr = this._scratchU32(0);
      const header = this.exports.silk_decode(ptr, len, ratePtr);
      if (header === 0) throw this._lastError();
      const samples = this._takeI16(header);
      const sampleRate = this._readU32(ratePtr) || this.detectSampleRate(bytes);
      return {
        samples,
        sampleRate,
        duration: samples.length / sampleRate,
        /** The samples as floats in [-1, 1), ready for Web Audio. */
        toFloat32: () => float32From(samples),
      };
    });
  }

  /**
   * Decode a complete SILK file to a 16-bit mono WAV file.
   *
   * @param {Uint8Array} bytes
   * @returns {Uint8Array}
   */
  decodeToWav(bytes) {
    return this._withBytes(bytes, (ptr, len) => {
      const header = this.exports.silk_decode_wav(ptr, len);
      if (header === 0) throw this._lastError();
      return this._takeBytes(header);
    });
  }

  /**
   * Create an incremental decoder for streaming playback.
   *
   * @returns {SilkStream}
   */
  stream() {
    return new SilkStream(this);
  }

  // ---- internals ---------------------------------------------------------

  /** Copy `bytes` into wasm memory, run `fn`, then free the copy. */
  _withBytes(bytes, fn) {
    const ptr = this.exports.silk_alloc(bytes.length || 1);
    if (ptr === 0) throw new SilkError('out of memory allocating input buffer');
    try {
      new Uint8Array(this.exports.memory.buffer, ptr, bytes.length).set(bytes);
      return fn(ptr, bytes.length);
    } finally {
      this.exports.silk_free(ptr, bytes.length || 1);
    }
  }

  _headerLength(bytes) {
    // "#!SILK_V3" optionally preceded by 0x02.
    if (bytes.length >= 10 && bytes[0] === 0x02) return 10;
    if (bytes.length >= 9) return 9;
    return 0;
  }

  /** Release the cached scratch area. */
  _freeScratch() {
    if (this._scratchPtr) {
      this.exports.silk_free(this._scratchPtr, 16);
      this._scratchPtr = 0;
      this._scratch = null;
    }
  }

  /** Release memory held by this loader. The instance stays usable afterwards. */
  dispose() {
    this._freeScratch();
  }

  /** A stable 4-byte scratch area for out-parameters. */
  _scratchU32(_index) {
    if (!this._scratch || this._scratch.buffer !== this.exports.memory.buffer) {
      // Reallocate after memory growth, which detaches old views.
      this._freeScratch();
      const ptr = this.exports.silk_alloc(16);
      if (ptr === 0) throw new SilkError('out of memory allocating scratch');
      this._scratch = new Uint32Array(this.exports.memory.buffer, ptr, 4);
      this._scratchPtr = ptr;
    }
    return this._scratchPtr;
  }

  _readU32(ptr) {
    return new DataView(this.exports.memory.buffer).getUint32(ptr, true);
  }

  /** Read an out-buffer of i16 samples and free it. */
  _takeI16(header) {
    const view = new DataView(this.exports.memory.buffer);
    const dataPtr = view.getUint32(header, true);
    const length = view.getUint32(header + 4, true);
    // Copy out before freeing, since the memory is about to be released.
    const samples = new Int16Array(length);
    const src = new Uint8Array(this.exports.memory.buffer, dataPtr, length * 2);
    new Uint8Array(samples.buffer).set(src);
    this.exports.silk_free_buffer(header);
    return samples;
  }

  /** Read an out-buffer of bytes and free it. */
  _takeBytes(header) {
    const view = new DataView(this.exports.memory.buffer);
    const dataPtr = view.getUint32(header, true);
    const length = view.getUint32(header + 4, true);
    const out = new Uint8Array(length);
    out.set(new Uint8Array(this.exports.memory.buffer, dataPtr, length));
    this.exports.silk_free_buffer(header);
    return out;
  }

  _lastError() {
    const lenPtr = this._scratchU32(0);
    const header = this.exports.silk_last_error(lenPtr);
    if (header === 0) return new SilkError('unknown decoder error');
    const message = new TextDecoder().decode(this._takeBytes(header));
    return new SilkError(message || 'unknown decoder error');
  }
}

/**
 * Incremental decoder over the packets of one SILK stream.
 *
 * Feed packets with {@link SilkStream#decode} as they arrive, or walk a
 * whole-file buffer with {@link SilkStream#decodeNext}. Each call returns the
 * PCM for that packet, which is normally one 20 ms frame.
 */
export class SilkStream {
  /** @param {Silk} silk */
  constructor(silk) {
    this.silk = silk;
    this.exports = silk.exports;
    this.handle = this.exports.silk_stream_new();
    if (this.handle === 0) throw new SilkError('out of memory creating stream decoder');
    this._disposed = false;
  }

  /** Sample rate in Hz. Valid after the first packet. */
  get sampleRate() {
    return this.exports.silk_stream_sample_rate(this.handle);
  }

  /** Number of packets decoded so far. */
  get packetsSeen() {
    return this.exports.silk_stream_packets(this.handle);
  }

  /**
   * Decode one packet.
   * @param {Uint8Array} packet
   * @returns {Int16Array}
   */
  decode(packet) {
    this.#assertLive();
    return this.silk._withBytes(packet, (ptr, len) => {
      const header = this.exports.silk_stream_decode(this.handle, ptr, len);
      if (header === 0) throw this.silk._lastError();
      return this.silk._takeI16(header);
    });
  }

  /**
   * Decode the next packet of a whole-file buffer.
   *
   * The stream keeps its own cursor, so repeated calls walk the file. Returns an
   * empty array once the end is reached, which makes it a natural loop condition:
   *
   * ```js
   * const stream = silk.stream();
   * for (;;) {
   *   const pcm = stream.decodeNext(bytes);
   *   if (pcm.length === 0) break;
   *   onFrame(pcm, stream.sampleRate);
   * }
   * ```
   *
   * @param {Uint8Array} bytes
   * @returns {Int16Array}
   */
  decodeNext(bytes) {
    this.#assertLive();
    this._cursor ??= this.silk._headerLength(bytes);
    return this.silk._withBytes(bytes, (ptr, len) => {
      const advancePtr = this.silk._scratchU32(0);
      const header = this.exports.silk_stream_decode_at(
        this.handle,
        ptr,
        len,
        this._cursor,
        advancePtr,
      );
      if (header === 0) {
        // Either the end of the file or a corrupt packet; distinguish by bounds.
        if (this._cursor + 2 > bytes.length) return new Int16Array(0);
        throw this.silk._lastError();
      }
      this._cursor += this.silk._readU32(advancePtr);
      return this.silk._takeI16(header);
    });
  }

  /** Release the underlying decoder. Safe to call more than once. */
  dispose() {
    if (!this._disposed) {
      this.exports.silk_stream_free(this.handle);
      this._disposed = true;
    }
  }

  #assertLive() {
    if (this._disposed) throw new SilkError('stream decoder has been disposed');
  }
}

/**
 * Load the WASM module and return a {@link Silk} decoder.
 *
 * @param {object} [options]
 * @param {string|URL} [options.wasmUrl] override the wasm location
 * @param {ArrayBuffer|Uint8Array} [options.wasmBytes] supply the wasm directly,
 *   which avoids any fetch (useful for bundlers and for Node)
 * @returns {Promise<Silk>}
 */
export async function loadSilk(options = {}) {
  let source = options.wasmBytes;
  if (!source) {
    const url = options.wasmUrl ?? DEFAULT_WASM_URL;
    if (typeof fetch === 'function' && String(url).startsWith('http')) {
      const res = await fetch(url);
      if (!res.ok) throw new SilkError(`failed to fetch ${url}: ${res.status}`);
      source = await res.arrayBuffer();
    } else {
      // Node, or any environment where the wasm sits on disk next to this file.
      const { readFile } = await import('node:fs/promises');
      source = await readFile(url instanceof URL ? url : new URL(url, import.meta.url));
    }
  }
  const bytes = source instanceof Uint8Array ? source : new Uint8Array(source);
  const { instance } = await WebAssembly.instantiate(bytes, {});
  return new Silk(instance);
}

/**
 * Decode a SILK file and hand back an `AudioBuffer` plus the raw samples.
 *
 * @param {Silk} silk a loaded decoder
 * @param {Uint8Array} bytes
 * @param {BaseAudioContext} [context] context to create the buffer in
 * @returns {{audioBuffer: AudioBuffer, samples: Int16Array, sampleRate: number, duration: number}}
 */
export async function decodeToAudioBuffer(silk, bytes, context) {
  const { samples, sampleRate, duration } = silk.decode(bytes);
  const ctx = context ?? new AudioContext();
  const audioBuffer = ctx.createBuffer(1, samples.length, sampleRate);
  const channel = audioBuffer.getChannelData(0);
  for (let i = 0; i < samples.length; i++) {
    channel[i] = samples[i] / 32768;
  }
  return { audioBuffer, samples, sampleRate, duration, context: ctx };
}

/**
 * Decode and play a SILK file through the Web Audio API.
 *
 * The browser resamples the native SILK rate to the output device, so no explicit
 * resampling step is needed and playback stays bit-accurate up to the sound card.
 *
 * @param {{samples: Int16Array, sampleRate: number}} audio as returned by {@link Silk#decode}
 * @param {object} [options]
 * @param {BaseAudioContext} [options.context] reuse an existing context
 * @param {number} [options.gain=1] playback gain
 * @returns {Promise<{source: AudioBufferSourceNode, context: BaseAudioContext, stop: () => void}>}
 */
export async function playSilk(audio, options = {}) {
  const ctx = options.context ?? new AudioContext();
  // A fresh context starts suspended under browser autoplay rules.
  if (ctx.state === 'suspended') await ctx.resume();

  const buffer = ctx.createBuffer(1, audio.samples.length, audio.sampleRate);
  const channel = buffer.getChannelData(0);
  for (let i = 0; i < audio.samples.length; i++) {
    channel[i] = audio.samples[i] / 32768;
  }

  const source = ctx.createBufferSource();
  source.buffer = buffer;
  if (options.gain !== undefined && options.gain !== 1) {
    const g = ctx.createGain();
    g.gain.value = options.gain;
    source.connect(g).connect(ctx.destination);
  } else {
    source.connect(ctx.destination);
  }
  source.start();
  return { source, context: ctx, stop: () => source.stop() };
}

/**
 * Streaming playback: decode packet by packet and schedule each frame as soon as
 * it is ready, so audio starts after the first 20 ms rather than after the whole
 * file has downloaded.
 *
 * @param {Silk} silk a loaded decoder
 * @param {object} [options]
 * @param {BaseAudioContext} [options.context]
 * @param {number} [options.gain=1]
 * @param {(frame: Int16Array, index: number) => void} [options.onFrame] progress hook
 * @returns {Promise<{context: BaseAudioContext, stop: () => void, finished: Promise<void>,
 *   push: (packet: Uint8Array) => void, end: () => void}>}
 */
export async function createSilkStreamPlayer(silk, options = {}) {
  const ctx = options.context ?? new AudioContext();
  if (ctx.state === 'suspended') await ctx.resume();

  const gainNode = ctx.createGain();
  gainNode.gain.value = options.gain ?? 1;
  gainNode.connect(ctx.destination);

  const stream = silk.stream();
  let nextTime = ctx.currentTime + 0.05; // small lead-in to absorb scheduling jitter
  let index = 0;
  let stopped = false;
  let resolveFinished;
  const finished = new Promise((resolve) => {
    resolveFinished = resolve;
  });

  const push = (packet) => {
    if (stopped) return;
    const pcm = stream.decode(packet);
    if (pcm.length === 0) return;
    const rate = stream.sampleRate;
    const buffer = ctx.createBuffer(1, pcm.length, rate);
    const channel = buffer.getChannelData(0);
    for (let i = 0; i < pcm.length; i++) channel[i] = pcm[i] / 32768;

    const source = ctx.createBufferSource();
    source.buffer = buffer;
    source.connect(gainNode);
    // Keep a small cushion so frames play back-to-back without gaps.
    if (nextTime < ctx.currentTime) nextTime = ctx.currentTime + 0.02;
    source.start(nextTime);
    nextTime += buffer.duration;
    options.onFrame?.(pcm, index++);
  };

  const end = () => {
    const remainingMs = Math.max(0, (nextTime - ctx.currentTime) * 1000) + 50;
    setTimeout(() => {
      stream.dispose();
      resolveFinished();
    }, remainingMs);
  };

  return {
    context: ctx,
    push,
    end,
    finished,
    stop: () => {
      stopped = true;
      try {
        gainNode.disconnect();
      } catch {
        /* already disconnected */
      }
      stream.dispose();
      resolveFinished();
    },
  };
}
