#![forbid(unsafe_code)]

mod biquad;
mod cng;
mod decode_core;
mod decode_parameters;
mod decoder;
mod fixed;
mod gains;
mod math;
mod nlsf;
mod nlsf2a;
mod pitch;
mod plc;
mod pulses_codec;
mod range_coder;
mod resampler;
mod resampler_tables;
mod sdk_api;
mod shell_coder;
mod tables_control;
mod tables_ltp;
mod tables_nlsf;
mod tables_nlsf_vec;
mod tables_pitch_lag;
mod tables_pulses;

pub use decoder::{DecoderControl, DecoderState, MAX_LPC_ORDER, NB_SUBFR};
pub use decode_parameters::peek_packet_fs_khz;
pub use sdk_api::{
    decode_silk, decode_silk_native, detect_sample_rate, list_packets, samples_per_packet,
    silk_header_len, silk_to_wav, wrap_wav, Decoder, SilkError,
};
