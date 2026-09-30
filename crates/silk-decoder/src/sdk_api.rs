use crate::biquad::biquad;
use crate::cng::cng;
use crate::decode_core::decode_core;
use crate::decode_parameters::decode_parameters;
use crate::decoder::{DecoderControl, DecoderState, NB_SUBFR};
use crate::plc::{plc, plc_glue_frames};
use crate::range_coder::{MAX_ARITHM_BYTES, RANGE_CODER_DEC_PAYLOAD_TOO_LONG};
use crate::resampler::SimpleResampler;
use thiserror::Error;

const DEC_INVALID_SAMPLING_FREQUENCY: i32 = -10;
/// Sentinel returned when the requested output rate is not one the SDK implements.
const DEC_UNSUPPORTED_RATE: i32 = -13;
const DEC_PAYLOAD_TOO_LARGE: i32 = -11;
const DEC_PAYLOAD_ERROR: i32 = -12;

#[derive(Clone, Copy, Debug, Default)]
struct DecControl {
    api_sample_rate: i32,
    frame_size: i32,
    frames_per_packet: i32,
    more_internal_decoder_frames: i32,
    in_band_fec_offset: i32,
}

const MORE_FRAMES: i32 = 1;
const LAST_FRAME: i32 = 0;
const LBRR_VER1: i32 = 2;
const LBRR_VER2: i32 = 3;
const NO_LBRR_THRES: i32 = 10;

#[derive(Default)]
pub struct SilkDecoder {
    pub state: DecoderState,
    resampler: Option<SimpleResampler>,
    resampler_in_hz: u32,
    resampler_out_hz: u32,
}

impl SilkDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn map_rc_error(err: i32) -> i32 {
        if err == RANGE_CODER_DEC_PAYLOAD_TOO_LONG {
            DEC_PAYLOAD_TOO_LARGE
        } else {
            DEC_PAYLOAD_ERROR
        }
    }

    fn on_payload_error(
        &mut self,
        action: &mut i32,
        used_bytes: &mut usize,
        ret: &mut i32,
        restore_fs_khz: Option<i32>,
    ) {
        self.state.n_bytes_left = 0;
        *action = 1;
        if let Some(fs_khz) = restore_fs_khz {
            self.state.set_fs(fs_khz);
        }
        *used_bytes = self.state.src.buffer_length;
        *ret = Self::map_rc_error(self.state.src.error);
    }

    fn update_fec_state(&mut self) {
        if self.state.vad_flag != 1 {
            return;
        }

        match self.state.frame_termination {
            LAST_FRAME => {
                self.state.no_fec_counter += 1;
                if self.state.no_fec_counter > NO_LBRR_THRES {
                    self.state.inband_fec_offset = 0;
                }
            }
            LBRR_VER1 => {
                self.state.inband_fec_offset = 1;
                self.state.no_fec_counter = 0;
            }
            LBRR_VER2 => {
                self.state.inband_fec_offset = 2;
                self.state.no_fec_counter = 0;
            }
            _ => {}
        }
    }

    fn maybe_resample(&mut self, input: &[i16], api_hz: i32) -> Result<Vec<i16>, i32> {
        let in_hz = (self.state.fs_khz * 1000) as u32;
        let out_hz = api_hz as u32;
        if in_hz == out_hz {
            return Ok(input.to_vec());
        }
        if self.resampler.is_none()
            || self.resampler_in_hz != in_hz
            || self.resampler_out_hz != out_hz
        {
            self.resampler = Some(
                SimpleResampler::new(in_hz, out_hz).map_err(|_| DEC_INVALID_SAMPLING_FREQUENCY)?,
            );
            self.resampler_in_hz = in_hz;
            self.resampler_out_hz = out_hz;
        }
        let rs = self
            .resampler
            .as_mut()
            .ok_or(DEC_INVALID_SAMPLING_FREQUENCY)?;
        let out_len = rs
            .output_len_for_input(input.len())
            .map_err(|_| DEC_INVALID_SAMPLING_FREQUENCY)?;
        let mut out = vec![0i16; out_len];
        let n = rs
            .process(input, &mut out)
            .map_err(|_| DEC_INVALID_SAMPLING_FREQUENCY)?;
        out.truncate(n);
        Ok(out)
    }

    fn decode_frame(
        &mut self,
        dec_control: &mut DecControl,
        lost_flag: i32,
        in_data: &[u8],
    ) -> Result<Vec<i16>, i32> {
        if self.state.more_internal_decoder_frames == 0 {
            self.state.n_frames_decoded = 0;
        }

        let mut action = if lost_flag != 0 { 1 } else { 0 };
        let mut ret = 0i32;
        if self.state.more_internal_decoder_frames == 0
            && action == 0
            && in_data.len() > MAX_ARITHM_BYTES
        {
            action = 1;
            ret = DEC_PAYLOAD_TOO_LARGE;
        }

        let prev_fs_khz = self.state.fs_khz;
        let mut ctrl = DecoderControl::default();
        let mut used_bytes = 0usize;
        let mut internal_pcm = vec![0i16; self.state.frame_length];

        if action == 0 {
            let fs_khz_old = self.state.fs_khz;
            if self.state.n_frames_decoded == 0 {
                self.state.src.dec_init(in_data);
            }

            if self.state.src.error != 0 {
                self.on_payload_error(&mut action, &mut used_bytes, &mut ret, None);
            } else {
                let out = decode_parameters(&mut self.state, &mut ctrl, true);
                if self.state.src.error != 0 {
                    self.on_payload_error(&mut action, &mut used_bytes, &mut ret, Some(fs_khz_old));
                } else {
                    used_bytes = self
                        .state
                        .src
                        .buffer_length
                        .saturating_sub(self.state.n_bytes_left as usize);
                    self.state.n_frames_decoded += 1;
                    internal_pcm = decode_core(&mut self.state, &mut ctrl, &out.q);
                    let l = self.state.frame_length;
                    plc(&mut self.state, &mut ctrl, &mut internal_pcm[..l], l, false);
                    self.state.loss_cnt = 0;
                    self.state.prev_sigtype = ctrl.sigtype;
                    self.state.lag_prev = ctrl.pitch_l[NB_SUBFR - 1];
                    self.state.first_frame_after_reset = 0;
                }
            }
        }

        let l = self.state.frame_length;
        if action == 1 {
            if internal_pcm.len() < l {
                internal_pcm.resize(l, 0);
            }
            plc(&mut self.state, &mut ctrl, &mut internal_pcm[..l], l, true);
        }

        self.state.out_buf[..l].copy_from_slice(&internal_pcm[..l]);
        plc_glue_frames(&mut self.state, &mut ctrl, &mut internal_pcm[..l], l);
        cng(&mut self.state, &mut ctrl, &mut internal_pcm[..l], l);

        let hp_in = internal_pcm[..l].to_vec();
        biquad(
            &hp_in,
            &self.state.hp_b,
            &self.state.hp_a,
            &mut self.state.hp_state,
            &mut internal_pcm[..l],
        );
        internal_pcm.truncate(l);

        if used_bytes != 0 {
            if self.state.n_bytes_left > 0
                && self.state.frame_termination == MORE_FRAMES
                && self.state.n_frames_decoded < 5
            {
                self.state.more_internal_decoder_frames = 1;
            } else {
                self.state.more_internal_decoder_frames = 0;
                self.state.n_frames_in_packet = self.state.n_frames_decoded;
                self.update_fec_state();
            }
        }

        if !(8_000..=48_000).contains(&dec_control.api_sample_rate) {
            return Err(DEC_INVALID_SAMPLING_FREQUENCY);
        }
        let pcm = self.maybe_resample(&internal_pcm, dec_control.api_sample_rate)?;
        self.state.prev_api_sample_rate = dec_control.api_sample_rate;
        // NOTE: the resampler is invalidated inside `maybe_resample`, which compares
        // the *internal* (fs_kHz) and API rates. An extra `prev_fs_khz != fs_khz`
        // check here used to drop a resampler that had just been created for this
        // very frame, so its IIR/FIR history restarted on every frame boundary and
        // the first samples of frame 1 did not match the reference decoder.
        let _ = prev_fs_khz;

        dec_control.frame_size = dec_control.api_sample_rate / 50;
        dec_control.frames_per_packet = self.state.n_frames_in_packet.max(1);
        dec_control.more_internal_decoder_frames = self.state.more_internal_decoder_frames;
        dec_control.in_band_fec_offset = self.state.inband_fec_offset;

        if ret != 0 {
            Err(ret)
        } else {
            Ok(pcm)
        }
    }
}

/// Length of the SILK container header, if `data` starts with one.
pub fn silk_header_len(data: &[u8]) -> Option<usize> {
    silk_header_len_inner(data)
}

/// Split a SILK container into its packets, in order.
pub fn list_packets(data: &[u8]) -> Vec<&[u8]> {
    let mut i = match silk_header_len_inner(data) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut packets = Vec::new();
    while i + 2 <= data.len() {
        let n = i16::from_le_bytes([data[i], data[i + 1]]) as i32;
        i += 2;
        if n <= 0 {
            break;
        }
        let n = n as usize;
        if i + n > data.len() {
            break;
        }
        packets.push(&data[i..i + n]);
        i += n;
    }
    packets
}

/// Native sample rate of a SILK stream, in Hz.
///
/// Every packet is self-describing, so the rate is read from the first packet's
/// header. Decoding at this rate requires no resampling and is therefore
/// bit-accurate against the reference decoder.
pub fn detect_sample_rate(data: &[u8]) -> Result<u32, SilkError> {
    let packet = *list_packets(data).first().ok_or(SilkError::InvalidOrEmptySilkFile)?;
    let rate = crate::decode_parameters::peek_packet_fs_khz(packet).ok_or(SilkError::InvalidOrEmptySilkFile)?;
    Ok((rate as u32) * 1000)
}

/// True when `hz` is a rate the built-in resampler can produce.
fn supported_rate(hz: u32) -> bool {
    crate::resampler::SUPPORTED_RATES.contains(&hz)
}

/// Decode a SILK stream at its own native rate, returning `(pcm, sample_rate)`.
///
/// The rate is read from the stream itself, so no resampling occurs and the output
/// is bit-accurate against the SILK reference decoder. The returned rate is the
/// rate actually used, which makes this safe even if a stream changes rate
/// mid-file.
pub fn decode_silk_native(data: &[u8]) -> Result<(Vec<i16>, u32), SilkError> {
    let packets = list_packets(data);
    if packets.is_empty() {
        return Err(SilkError::InvalidOrEmptySilkFile);
    }
    let mut decoder = Decoder::new(0);
    let mut samples = Vec::with_capacity(packets.len() * 480);
    for packet in packets {
        samples.extend_from_slice(&decoder.decode_packet(packet)?);
    }
    if samples.is_empty() {
        return Err(SilkError::NoAudioDecoded);
    }
    Ok((samples, decoder.sample_rate()))
}

fn silk_header_len_inner(data: &[u8]) -> Option<usize> {
    if data.starts_with(b"\x02#!SILK_V3") {
        Some(1 + b"#!SILK_V3".len())
    } else if data.starts_with(b"#!SILK_V3") {
        Some(b"#!SILK_V3".len())
    } else if data.starts_with(b"!SILK_V3") {
        Some(b"!SILK_V3".len())
    } else {
        None
    }
}

fn parse_silk_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut i = match silk_header_len(data) {
        Some(v) => v,
        None => return Vec::new(),
    };

    let mut packets = Vec::new();
    while i + 2 <= data.len() {
        let n = i16::from_le_bytes([data[i], data[i + 1]]) as i32;
        i += 2;
        if n <= 0 {
            break;
        }
        let n = n as usize;
        if i + n > data.len() {
            break;
        }
        packets.push(data[i..i + n].to_vec());
        i += n;
    }
    packets
}

fn build_wav_header(sample_rate: u32, num_samples: usize) -> Vec<u8> {
    let data_size = num_samples * 2;
    let file_size = 36 + data_size;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(file_size as u32).to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(data_size as u32).to_le_bytes());
    header
}

#[derive(Debug, Error)]
pub enum SilkError {
    #[error("invalid sample rate, must be in [8000, 48000]")]
    InvalidSampleRate,
    #[error(
        "cannot resample {native} Hz (the stream's rate) to {requested} Hz: \
         supported output rates are 8000, 12000, 16000, 24000, 32000 and 48000 Hz. \
         Decode at the native rate and resample afterwards instead"
    )]
    UnsupportedSampleRate { native: u32, requested: u32 },
    #[error("SILK packet exceeds the maximum payload size")]
    PacketTooLarge,
    #[error("corrupt SILK packet")]
    CorruptPacket,
    #[error("SILK decoder error {0}")]
    Decode(i32),
    #[error("failed to read file: {0}")]
    ReadFile(#[from] std::io::Error),
    #[error("invalid or empty SILK stream (no #!SILK_V3 header or no packets)")]
    InvalidOrEmptySilkFile,
    #[error("no audio decoded from SILK stream")]
    NoAudioDecoded,
}

pub fn silk_to_wav(sample_rate: u32, silk_path: &str) -> Result<Vec<u8>, SilkError> {
    let data = std::fs::read(silk_path)?;
    let pcm = decode_silk(&data, sample_rate)?;
    Ok(wrap_wav(sample_rate, &pcm))
}

/// Number of PCM samples the decoder emits for one SILK packet, given the requested
/// API sample rate. Used by callers to size buffers up front.
pub fn samples_per_packet(api_sample_rate: u32) -> usize {
    // SILK frames are 20 ms; a packet carries 1..=5 frames.
    (api_sample_rate as usize / 50) * 5
}

/// Decode a complete SILK file held in memory into 16-bit mono PCM.
///
/// `data` is the raw `.slk` / `.silk` byte stream, including the `#!SILK_V3` header.
/// `api_sample_rate` is the desired output rate in Hz and must be within
/// 8000..=48000 (the decoder resamples internally when it differs from the
/// packet's native rate).
pub fn decode_silk(data: &[u8], api_sample_rate: u32) -> Result<Vec<i16>, SilkError> {
    if !(8000..=48000).contains(&api_sample_rate) {
        return Err(SilkError::InvalidSampleRate);
    }
    let native = detect_sample_rate(data)?;
    if native != api_sample_rate && !supported_rate(api_sample_rate) {
        return Err(SilkError::UnsupportedSampleRate {
            native,
            requested: api_sample_rate,
        });
    }

    let packets = parse_silk_packets(data);
    if packets.is_empty() {
        return Err(SilkError::InvalidOrEmptySilkFile);
    }

    let mut decoder = SilkDecoder::new();
    let mut control = DecControl {
        api_sample_rate: api_sample_rate as i32,
        frames_per_packet: 1,
        ..Default::default()
    };

    let mut pcm_samples: Vec<i16> =
        Vec::with_capacity(packets.len() * samples_per_packet(api_sample_rate));
    for packet in &packets {
        while let Ok(samples) = decoder.decode_frame(&mut control, 0, packet) {
            pcm_samples.extend_from_slice(&samples);
            if control.more_internal_decoder_frames == 0 {
                break;
            }
        }
    }

    if pcm_samples.is_empty() {
        return Err(SilkError::NoAudioDecoded);
    }

    Ok(pcm_samples)
}

/// Wrap 16-bit mono PCM into a canonical 44-byte RIFF/WAVE container.
pub fn wrap_wav(sample_rate: u32, pcm: &[i16]) -> Vec<u8> {
    let mut wav = build_wav_header(sample_rate, pcm.len());
    for s in pcm {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// Incremental (streaming) decoder over individual SILK packets.
///
/// This is the stateful counterpart of [`decode_silk`]: call [`Decoder::decode_packet`]
/// once per packet, in order. It is what the WASM layer uses so that audio can be
/// decoded and played progressively instead of buffering the whole file.
pub struct Decoder {
    inner: SilkDecoder,
    control: DecControl,
    api_sample_rate: u32,
    /// When set, the output rate tracks each packet's declared rate.
    follow_native_rate: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new(24_000)
    }
}

impl Decoder {
    /// Create a decoder.
    ///
    /// Pass `0` to follow the stream's own rate: each packet is decoded at whatever
    /// rate it declares, which needs no resampling and is therefore bit-accurate.
    /// A non-zero rate forces resampling to that rate.
    pub fn new(api_sample_rate: u32) -> Self {
        // Mirror SILK's default rate until the first packet reveals the real one.
        let initial = if api_sample_rate == 0 { 24_000 } else { api_sample_rate };
        Self {
            inner: SilkDecoder::new(),
            control: DecControl {
                api_sample_rate: initial as i32,
                frames_per_packet: 1,
                ..Default::default()
            },
            api_sample_rate: initial,
            follow_native_rate: api_sample_rate == 0,
        }
    }

    /// Change the output sample rate; takes effect from the next packet.
    pub fn set_sample_rate(&mut self, api_sample_rate: u32) {
        self.control.api_sample_rate = api_sample_rate as i32;
        self.api_sample_rate = api_sample_rate;
        self.follow_native_rate = false;
    }

    /// Decode one SILK packet into 16-bit mono PCM.
    ///
    /// A packet normally yields one 20 ms frame, but up to five frames when the
    /// encoder packed several together, so the returned vector may be up to five
    /// 20 ms blocks long.
    pub fn decode_packet(&mut self, packet: &[u8]) -> Result<Vec<i16>, SilkError> {
        let native = crate::decode_parameters::peek_packet_fs_khz(packet)
            .map(|fs| fs as u32 * 1000)
            .unwrap_or(0);
        if self.follow_native_rate {
            if native != 0 && native != self.api_sample_rate {
                self.set_sample_rate(native);
            }
        } else if native != 0 && native != self.api_sample_rate && !supported_rate(self.api_sample_rate)
        {
            return Err(SilkError::UnsupportedSampleRate {
                native,
                requested: self.api_sample_rate,
            });
        }
        let mut out = Vec::new();
        loop {
            match self.inner.decode_frame(&mut self.control, 0, packet) {
                Ok(samples) => {
                    out.extend_from_slice(&samples);
                    if self.control.more_internal_decoder_frames == 0 {
                        break;
                    }
                }
                Err(code) => {
                    // A packet-level decode error. Keep whatever was produced, but
                    // report the failure if there is nothing at all.
                    if out.is_empty() {
                        return Err(self.map_frame_error(code));
                    }
                    break;
                }
            }
        }
        if out.is_empty() {
            return Err(SilkError::NoAudioDecoded);
        }
        Ok(out)
    }

    /// Translate a SILK decoder status code into a typed error.
    fn map_frame_error(&self, code: i32) -> SilkError {
        match code {
            DEC_UNSUPPORTED_RATE => SilkError::UnsupportedSampleRate {
                requested: self.api_sample_rate,
                native: self.inner.state.fs_khz as u32 * 1000,
            },
            -10 => SilkError::UnsupportedSampleRate {
                requested: self.api_sample_rate,
                native: self.inner.state.fs_khz as u32 * 1000,
            },
            -11 => SilkError::PacketTooLarge,
            -12 => SilkError::CorruptPacket,
            _ => SilkError::Decode(code),
        }
    }

    /// Decode a packet that was lost, using the built-in packet loss concealment.
    pub fn decode_lost_packet(&mut self) -> Vec<i16> {
        let mut out = Vec::new();
        while let Ok(samples) = self.inner.decode_frame(&mut self.control, 1, &[]) {
            out.extend_from_slice(&samples);
            if self.control.more_internal_decoder_frames == 0 {
                break;
            }
        }
        out
    }

    /// Number of output samples per 20 ms frame at the current rate.
    pub fn frame_size(&self) -> usize {
        self.api_sample_rate as usize / 50
    }

    /// Debug helper: print resampler internal state.
    pub fn dump_resampler(&self) {
        if let Some(rs) = &self.inner.resampler {
            rs.dump_state();
        } else {
            eprintln!("  [RR] resampler is None");
        }
    }

    /// Sample rate of the PCM this decoder is producing, in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.api_sample_rate
    }

    /// True when this decoder follows the stream's own rate rather than resampling.
    pub fn follows_native_rate(&self) -> bool {
        self.follow_native_rate
    }

    /// Read-only access to the underlying decoder state (used by the CLI for
    /// bit-exact comparison against the C reference decoder).
    pub fn state(&self) -> &DecoderState {
        &self.inner.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_wav_writes_canonical_header() {
        let pcm = [0i16, 1, -1, 32767, -32768];
        let wav = wrap_wav(24_000, &pcm);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + pcm.len() * 2);
        // sample rate is stored little-endian at offset 24
        assert_eq!(u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]), 24_000);
    }
}


