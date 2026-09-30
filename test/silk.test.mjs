/**
 * Test suite for the SILK WebAssembly decoder.
 *
 * Run with:  node test/silk.test.mjs
 *
 * `test/fixtures/test_16000.pcm` is raw 16-bit mono PCM produced by the SILK
 * reference decoder (Skype's SILK SDK v1.0.9, fixed-point build) for
 * `test/fixtures/test.slk`. Comparing against it proves the Rust port is
 * bit-exact rather than merely close.
 */

import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { loadSilk, SilkError } from '../web/silk.js';

const here = dirname(fileURLToPath(import.meta.url));
const fixture = (name) => join(here, 'fixtures', name);

let passed = 0;
let failed = 0;

function check(name, fn) {
  try {
    fn();
    passed++;
    console.log(`  \u2713 ${name}`);
  } catch (e) {
    failed++;
    console.log(`  \u2717 ${name}`);
    console.log(`      ${e.message}`);
  }
}

function assert(cond, msg) {
  if (!cond) throw new Error(msg);
}

function assertEqual(actual, expected, msg) {
  if (actual !== expected) {
    throw new Error(`${msg ?? 'values differ'}: expected ${expected}, got ${actual}`);
  }
}

/** Count differing samples between two Int16Arrays. */
function diffCount(a, b) {
  if (a.length !== b.length) return -1;
  let d = 0;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) d++;
  return d;
}

const silk = await loadSilk();
const slk = new Uint8Array(await readFile(fixture('test.slk')));
const reference = new Int16Array(
  (await readFile(fixture('test_16000.pcm'))).buffer.slice(0),
);

console.log('\nSILK WASM decoder\n');

console.log('container');
check('recognises the #!SILK_V3 header', () => {
  assert(silk.isSilk(slk), 'isSilk returned false');
  assert(!silk.isSilk(new Uint8Array([1, 2, 3, 4])), 'false positive on junk input');
});
check('detects the stream sample rate', () => {
  assertEqual(silk.detectSampleRate(slk), 16000, 'sample rate');
});
check('enumerates packets', () => {
  const packets = silk.packets(slk);
  assertEqual(packets.length, 250, 'packet count');
  // Packet payloads must tile the file after the 10-byte header and 2-byte lengths.
  const total = packets.reduce((n, p) => n + p.length, 0) + 2 * packets.length + 10;
  assertEqual(total, slk.length, 'packets must account for every byte');
});

console.log('\ndecoding');
check('decodes the whole file to the expected length', () => {
  const audio = silk.decode(slk);
  assertEqual(audio.sampleRate, 16000, 'sample rate');
  assertEqual(audio.samples.length, 80000, 'sample count');
  assert(Math.abs(audio.duration - 5.0) < 1e-9, `duration should be 5s, got ${audio.duration}`);
});
check('matches the C reference decoder sample-for-sample', () => {
  const audio = silk.decode(slk);
  const d = diffCount(audio.samples, reference);
  assert(d === 0, `${d} of ${reference.length} samples differ from the reference`);
});
check('is deterministic across calls', () => {
  const a = silk.decode(slk).samples;
  const b = silk.decode(slk).samples;
  assertEqual(diffCount(a, b), 0, 'repeat decode differs');
});
check('toFloat32 maps into [-1, 1)', () => {
  const audio = silk.decode(slk);
  const f = audio.toFloat32();
  assertEqual(f.length, audio.samples.length, 'float length');
  for (let i = 0; i < f.length; i++) {
    assert(f[i] >= -1 && f[i] < 1, `sample ${i} out of range: ${f[i]}`);
  }
});

console.log('\nstreaming');
check('streaming matches one-shot decoding exactly', () => {
  const oneShot = silk.decode(slk).samples;
  const stream = silk.stream();
  const parts = [];
  let total = 0;
  for (;;) {
    const pcm = stream.decodeNext(slk);
    if (pcm.length === 0) break;
    parts.push(pcm);
    total += pcm.length;
  }
  const joined = new Int16Array(total);
  let off = 0;
  for (const p of parts) {
    joined.set(p, off);
    off += p.length;
  }
  assertEqual(stream.packetsSeen, 250, 'packets consumed');
  assertEqual(stream.sampleRate, 16000, 'stream sample rate');
  assertEqual(diffCount(joined, oneShot), 0, 'streamed PCM differs from one-shot');
  assertEqual(diffCount(joined, reference), 0, 'streamed PCM differs from reference');
  stream.dispose();
});
check('per-packet decoding matches the concatenation', () => {
  const packets = silk.packets(slk);
  const stream = silk.stream();
  const joined = new Int16Array(packets.length * 320);
  let off = 0;
  for (const p of packets) {
    const pcm = stream.decode(p);
    joined.set(pcm, off);
    off += pcm.length;
  }
  assertEqual(off, 80000, 'total samples');
  assertEqual(diffCount(joined, reference), 0, 'per-packet PCM differs from reference');
  stream.dispose();
});
check('a disposed stream rejects further use', () => {
  const stream = silk.stream();
  stream.dispose();
  let threw = false;
  try {
    stream.decode(slk);
  } catch (e) {
    threw = e instanceof SilkError;
  }
  assert(threw, 'expected SilkError after dispose');
});

console.log('\nWAV output');
check('wraps PCM in a valid RIFF/WAVE container', () => {
  const wav = silk.decodeToWav(slk);
  const view = new DataView(wav.buffer, wav.byteOffset, wav.byteLength);
  const tag = (off) => String.fromCharCode(...wav.slice(off, off + 4));
  assertEqual(tag(0), 'RIFF', 'RIFF tag');
  assertEqual(tag(8), 'WAVE', 'WAVE tag');
  assertEqual(tag(12), 'fmt ', 'fmt tag');
  assertEqual(tag(36), 'data', 'data tag');
  assertEqual(view.getUint16(20, true), 1, 'PCM format');
  assertEqual(view.getUint16(22, true), 1, 'mono');
  assertEqual(view.getUint32(24, true), 16000, 'sample rate');
  assertEqual(view.getUint16(34, true), 16, 'bit depth');
  assertEqual(view.getUint32(40, true), 80000 * 2, 'data size');
  assertEqual(wav.length, 44 + 80000 * 2, 'file size');
  // The payload must be the reference samples, little-endian.
  const payload = new Int16Array(wav.buffer, wav.byteOffset + 44, 80000);
  assertEqual(diffCount(payload, reference), 0, 'WAV payload differs from reference');
});

console.log('\nerror handling');
check('rejects a buffer that is not SILK', () => {
  let err = null;
  try {
    silk.decode(new Uint8Array([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]));
  } catch (e) {
    err = e;
  }
  assert(err instanceof SilkError, 'expected a SilkError');
});
check('rejects an empty buffer', () => {
  let threw = false;
  try {
    silk.decode(new Uint8Array(0));
  } catch {
    threw = true;
  }
  assert(threw, 'expected an error for empty input');
});
check('rejects a truncated file without crashing', () => {
  const truncated = slk.slice(0, 5000);
  try {
    const audio = silk.decode(truncated);
    // Decoding a prefix is allowed, but it must not invent audio.
    assert(audio.samples.length <= 80000, 'decoded more audio than the file holds');
  } catch (e) {
    assert(e instanceof SilkError, 'expected a SilkError');
  }
});

console.log(`\n${passed} passed, ${failed} failed\n`);
process.exit(failed === 0 ? 0 : 1);
