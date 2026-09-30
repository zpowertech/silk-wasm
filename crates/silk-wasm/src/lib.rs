//! WebAssembly bindings for the pure-Rust SILK v3 decoder.
//!
//! Two levels of API are exposed:
//!
//! * [`SilkDecoder`] — a stateful, packet-by-packet decoder. Push the packets of a
//!   `.slk` file as they arrive and read PCM out as it is produced, which is what
//!   you want for streaming, or for starting playback before the file is complete.
//! * [`decode_silk`] — a one-shot helper for a complete in-memory file.
//!
//! Both produce 16-bit mono PCM at the stream's native rate (normally 24 kHz for
//! SILK v3, but 8/12/16 kHz are also valid). Hand those samples to Web Audio and
//! the browser resamples to the output device.

use wasm_bindgen::prelude::*;

/// Decoded audio returned by [`decode_silk`].
#[wasm_bindgen]
pub struct DecodedAudio {
    samples: Vec<i16>,
    sample_rate: u32,
}

#[wasm_bindgen]
impl DecodedAudio {
    /// PCM samples as an `Int16Array` (mono).
    #[wasm_bindgen(getter)]
    pub fn samples(&self) -> Vec<i16> {
        self.samples.clone()
    }

    /// Number of PCM samples.
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        self.samples.len()
    }

    /// Native sample rate of the decoded audio, in Hz.
    #[wasm_bindgen(getter, js_name = sampleRate)]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Duration in seconds.
    #[wasm_bindgen(getter, js_name = duration)]
    pub fn duration(&self) -> f64 {
        self.samples.len() as f64 / self.sample_rate as f64
    }

    /// Convert the samples to the `[-1, 1]` float range Web Audio uses.
    #[wasm_bindgen(js_name = toFloat32)]
    pub fn to_float32(&self) -> Vec<f32> {
        self.samples.iter().map(|&s| s as f32 / 32768.0).collect()
    }
}

/// True when `data` starts with a recognised `#!SILK_V3` header.
#[wasm_bindgen(js_name = isSilk)]
pub fn is_silk(data: &[u8]) -> bool {
    silk_decoder::silk_header_len(data).is_some()
}

/// Byte length of each SILK packet in `data`, in order.
///
/// Lets JavaScript walk the container itself (for example to hand packets to
/// [`SilkDecoder`] as they arrive over the network) without reimplementing the
/// format.
#[wasm_bindgen(js_name = listPackets)]
pub fn list_packets(data: &[u8]) -> Vec<u32> {
    silk_decoder::list_packets(data)
        .into_iter()
        .map(|p| p.len() as u32)
        .collect()
}

/// Native sample rate of a SILK stream in Hz, taken from its first packet.
///
/// This is the rate at which the decoder produces bit-accurate output.
#[wasm_bindgen(js_name = detectSampleRate)]
pub fn detect_sample_rate(data: &[u8]) -> Result<u32, JsValue> {
    silk_decoder::detect_sample_rate(data).map_err(to_js_error)
}

/// Decode a whole SILK file held in memory.
///
/// `data` must be the raw file bytes including the header.
///
/// ```js
/// const res = await fetch(url);
/// const bytes = new Uint8Array(await res.arrayBuffer());
/// const audio = decodeSilk(bytes);
/// console.log(audio.sampleRate, audio.duration);
/// ```
#[wasm_bindgen(js_name = decodeSilk)]
pub fn decode_silk(data: &[u8]) -> Result<DecodedAudio, JsValue> {
    let (samples, sample_rate) = silk_decoder::decode_silk_native(data).map_err(to_js_error)?;
    Ok(DecodedAudio {
        samples,
        sample_rate,
    })
}

fn to_js_error(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// Incremental SILK decoder.
///
/// Feed packets in order with [`SilkDecoder::decode`] and collect the PCM each one
/// produces. Driving the decoder this way lets playback start after the first
/// packet rather than waiting for the whole file.
#[wasm_bindgen]
pub struct SilkDecoder {
    inner: silk_decoder::Decoder,
    sample_rate: u32,
    packets_seen: u32,
    produced: usize,
    cursor: usize,
}

#[wasm_bindgen]
impl SilkDecoder {
    /// Create a decoder. The first decoded packet determines the sample rate.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: silk_decoder::Decoder::new(0),
            sample_rate: 24_000,
            packets_seen: 0,
            produced: 0,
            cursor: 0,
        }
    }

    /// Native sample rate in Hz; valid once one packet has been decoded.
    #[wasm_bindgen(getter, js_name = sampleRate)]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Number of packets decoded so far.
    #[wasm_bindgen(getter, js_name = packetsSeen)]
    pub fn packets_seen(&self) -> u32 {
        self.packets_seen
    }

    /// Total PCM samples produced so far.
    #[wasm_bindgen(getter, js_name = samplesProduced)]
    pub fn samples_produced(&self) -> usize {
        self.produced
    }

    /// Output samples per 20 ms frame at the current rate.
    #[wasm_bindgen(js_name = frameSize)]
    pub fn frame_size(&self) -> usize {
        self.inner.frame_size()
    }

    /// Decode one SILK packet, returning the PCM it produced.
    ///
    /// A packet normally yields one 20 ms frame, but up to five if the encoder
    /// packed several together, so the array length is a multiple of the frame size.
    #[wasm_bindgen(js_name = decode)]
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, JsValue> {
        let pcm = self.inner.decode_packet(packet).map_err(to_js_error)?;
        self.sample_rate = self.inner.sample_rate();
        self.packets_seen += 1;
        self.produced += pcm.len();
        Ok(pcm)
    }

    /// Decode the next packet out of a whole-file buffer, advancing an internal
    /// cursor. Returns an empty array once the file is exhausted.
    ///
    /// ```js
    /// const dec = new SilkDecoder();
    /// for (;;) {
    ///   const pcm = dec.decodeNext(bytes);
    ///   if (pcm.length === 0) break;
    ///   queue(pcm, dec.sampleRate);
    /// }
    /// ```
    #[wasm_bindgen(js_name = decodeNext)]
    pub fn decode_next(&mut self, file: &[u8]) -> Result<Vec<i16>, JsValue> {
        let packets = silk_decoder::list_packets(file);
        if self.cursor >= packets.len() {
            return Ok(Vec::new());
        }
        let packet = packets[self.cursor];
        self.cursor += 1;
        self.decode(packet)
    }

    /// Rewind the [`SilkDecoder::decode_next`] cursor and clear decoder state.
    #[wasm_bindgen(js_name = reset)]
    pub fn reset(&mut self) {
        self.inner = silk_decoder::Decoder::new(self.sample_rate);
        self.cursor = 0;
        self.packets_seen = 0;
        self.produced = 0;
    }

    /// Run packet loss concealment for a missing packet.
    #[wasm_bindgen(js_name = decodeLost)]
    pub fn decode_lost(&mut self) -> Vec<i16> {
        let pcm = self.inner.decode_lost_packet();
        self.produced += pcm.len();
        pcm
    }

    /// Wrap raw PCM into a 16-bit mono WAV container at `sampleRate`.
    #[wasm_bindgen(js_name = toWav)]
    pub fn to_wav(pcm: &[i16], sample_rate: u32) -> Vec<u8> {
        silk_decoder::wrap_wav(sample_rate, pcm)
    }
}

impl Default for SilkDecoder {
    fn default() -> Self {
        Self::new()
    }
}
