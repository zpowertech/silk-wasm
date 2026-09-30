//! Dependency-free WebAssembly core for the SILK v3 decoder.
//!
//! This crate deliberately avoids `wasm-bindgen`: it exports a small C-style ABI
//! over WebAssembly linear memory so the module can be built with nothing but
//! `cargo build --target wasm32-unknown-unknown` and instantiated from JavaScript
//! with a plain `WebAssembly.instantiate`. That keeps the build reproducible
//! offline and the delivery to a single `.wasm` file.
//!
//! # Calling convention
//!
//! Byte buffers and sample buffers are passed as `(ptr, len)` pairs into the
//! module's exported memory. Every function that returns a buffer returns the
//! pointer to a small header:
//!
//! ```text
//! [ data_ptr: u32 ][ length: u32 ][ capacity: u32 ][ reserved: u32 ]
//! ```
//!
//! `data_ptr` points at the payload (bytes or `i16` samples), `length` is the
//! number of elements, and `capacity` is what must be handed back to
//! [`silk_free_buffer`]. Header and payload are one allocation, so a single
//! `silk_free_buffer(header_ptr)` releases both.

use std::alloc::{alloc, dealloc, Layout};

use silk_decoder::{Decoder, SilkError};

/// Allocate `len` bytes and return the pointer. Returns 0 on failure.
///
/// The host calls this to copy input bytes into the module's memory.
#[no_mangle]
pub extern "C" fn silk_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    let layout = match Layout::from_size_align(len, 1) {
        Ok(l) => l,
        Err(_) => return std::ptr::null_mut(),
    };
    unsafe { alloc(layout) }
}

/// Release memory obtained from [`silk_alloc`].
///
/// # Safety
/// `ptr`/`len` must be exactly what was passed to a matching `silk_alloc`.
#[no_mangle]
pub unsafe extern "C" fn silk_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    if let Ok(layout) = Layout::from_size_align(len, 1) {
        dealloc(ptr, layout);
    }
}

/// Helpers for the heap buffers handed to the host: a 4-word header followed by
/// the payload, allocated as one block so a single free releases both.
struct OutBuffer;

impl OutBuffer {
    /// Move `bytes` into a freshly allocated out-buffer and return its header
    /// pointer, or 0 if allocation fails.
    fn from_bytes(bytes: &[u8]) -> *mut u32 {
        Self::from_raw(bytes.len(), 1, |dst| unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        })
    }

    /// Serialise `samples` as little-endian `i16` into an out-buffer.
    fn from_samples(samples: &[i16]) -> *mut u32 {
        let n = samples.len();
        Self::from_raw(n, 2, |dst| unsafe {
            for (i, s) in samples.iter().enumerate() {
                let b = s.to_le_bytes();
                std::ptr::copy_nonoverlapping(b.as_ptr(), dst.add(i * 2), 2);
            }
        })
    }

    /// Allocate a header plus `count * item_size` payload bytes and run `fill`.
    fn from_raw(count: usize, item_size: usize, fill: impl FnOnce(*mut u8)) -> *mut u32 {
        const HEADER_WORDS: usize = 4;
        let payload = match count.checked_mul(item_size) {
            Some(v) => v,
            None => return std::ptr::null_mut(),
        };
        let total = HEADER_WORDS * 4 + payload;
        let layout = match Layout::from_size_align(total, 4) {
            Ok(l) => l,
            Err(_) => return std::ptr::null_mut(),
        };
        let header = unsafe { alloc(layout) } as *mut u32;
        if header.is_null() {
            return std::ptr::null_mut();
        }
        let data = unsafe { header.add(HEADER_WORDS) } as *mut u8;
        unsafe {
            header.write((data as u32).to_le());
            header.add(1).write(count as u32);
            header.add(2).write(total as u32);
            header.add(3).write(0);
        }
        fill(data);
        header
    }
}

/// Release an out-buffer previously returned by any `silk_*` function.
///
/// # Safety
/// `header` must come from one of this module's exported functions and must not
/// be freed twice.
#[no_mangle]
pub unsafe extern "C" fn silk_free_buffer(header: *mut u32) {
    if header.is_null() {
        return;
    }
    let total = header.add(2).read() as usize;
    if let Ok(layout) = Layout::from_size_align(total, 4) {
        dealloc(header as *mut u8, layout);
    }
}

/// Describe the last error raised by a fallible call.
///
/// Returns an out-buffer holding a UTF-8 message and sets `out_len` to its byte
/// length. Call immediately after a call returned 0.
#[no_mangle]
pub unsafe extern "C" fn silk_last_error(out_len: *mut u32) -> *mut u32 {
    let msg = last_error_message();
    if !out_len.is_null() {
        *out_len = msg.len() as u32;
    }
    OutBuffer::from_bytes(msg.as_bytes())
}

thread_local! {
    static LAST_ERROR: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn set_last_error(e: &SilkError) {
    LAST_ERROR.with(|c| *c.borrow_mut() = e.to_string());
}

fn last_error_message() -> String {
    LAST_ERROR.with(|c| {
        let s = c.borrow();
        if s.is_empty() {
            "unknown error".to_string()
        } else {
            s.clone()
        }
    })
}

fn read_bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

/// True (1) when the buffer starts with a recognised `#!SILK_V3` header.
#[no_mangle]
pub extern "C" fn silk_is_silk(ptr: *const u8, len: usize) -> i32 {
    if silk_decoder::silk_header_len(read_bytes(ptr, len)).is_some() {
        1
    } else {
        0
    }
}

/// Native sample rate of the stream in Hz, or 0 on failure.
#[no_mangle]
pub extern "C" fn silk_detect_sample_rate(ptr: *const u8, len: usize) -> u32 {
    match silk_decoder::detect_sample_rate(read_bytes(ptr, len)) {
        Ok(r) => r,
        Err(e) => {
            set_last_error(&e);
            0
        }
    }
}

/// Number of packets in the stream, or 0 if it is not a SILK file.
#[no_mangle]
pub extern "C" fn silk_packet_count(ptr: *const u8, len: usize) -> u32 {
    silk_decoder::list_packets(read_bytes(ptr, len)).len() as u32
}

/// Byte length of packet `index`, or 0 when out of range.
#[no_mangle]
pub extern "C" fn silk_packet_len(ptr: *const u8, len: usize, index: u32) -> u32 {
    silk_decoder::list_packets(read_bytes(ptr, len))
        .get(index as usize)
        .map(|p| p.len() as u32)
        .unwrap_or(0)
}

/// Byte offset of packet `index` within the file, or 0 when out of range.
#[no_mangle]
pub extern "C" fn silk_packet_offset(ptr: *const u8, len: usize, index: u32) -> u32 {
    let data = read_bytes(ptr, len);
    let packets = silk_decoder::list_packets(data);
    let index = index as usize;
    if index >= packets.len() {
        return 0;
    }
    // Recompute the walk to find the offset of this packet's payload.
    let mut i = match silk_decoder::silk_header_len(data) {
        Some(v) => v,
        None => return 0,
    };
    for k in 0..=index {
        if i + 2 > data.len() {
            return 0;
        }
        let n = i16::from_le_bytes([data[i], data[i + 1]]) as i32;
        i += 2;
        if n <= 0 || i + n as usize > data.len() {
            return 0;
        }
        if k == index {
            return i as u32;
        }
        i += n as usize;
    }
    0
}

/// Decode a complete SILK file at its native rate.
///
/// Returns an out-buffer of `i16` PCM samples (little-endian in memory), or null
/// on failure. On success `*out_rate` receives the sample rate in Hz.
///
/// # Safety
/// `ptr`/`len` must describe a readable region of this module's memory and
/// `out_rate` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn silk_decode(
    ptr: *const u8,
    len: usize,
    out_rate: *mut u32,
) -> *mut u32 {
    match silk_decoder::decode_silk_native(read_bytes(ptr, len)) {
        Ok((pcm, rate)) => {
            if !out_rate.is_null() {
                *out_rate = rate;
            }
            OutBuffer::from_samples(&pcm)
        }
        Err(e) => {
            set_last_error(&e);
            std::ptr::null_mut()
        }
    }
}

/// Decode a complete SILK file, returning a 16-bit mono WAV file.
///
/// # Safety
/// `ptr`/`len` must describe a readable region of this module's memory.
#[no_mangle]
pub unsafe extern "C" fn silk_decode_wav(ptr: *const u8, len: usize) -> *mut u32 {
    match silk_decoder::decode_silk_native(read_bytes(ptr, len)) {
        Ok((pcm, rate)) => OutBuffer::from_bytes(&silk_decoder::wrap_wav(rate, &pcm)),
        Err(e) => {
            set_last_error(&e);
            std::ptr::null_mut()
        }
    }
}

/// Opaque incremental decoder handle used by the streaming entry points.
pub struct StreamDecoder {
    inner: Decoder,
    sample_rate: u32,
    seen: u32,
}

/// Create a streaming decoder. Release it with [`silk_stream_free`].
#[no_mangle]
pub extern "C" fn silk_stream_new() -> *mut StreamDecoder {
    Box::into_raw(Box::new(StreamDecoder {
        inner: Decoder::new(0),
        sample_rate: 24_000,
        seen: 0,
    }))
}

/// Destroy a streaming decoder created by [`silk_stream_new`].
///
/// # Safety
/// `handle` must come from `silk_stream_new` and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn silk_stream_free(handle: *mut StreamDecoder) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

/// Sample rate the decoder is currently producing, in Hz.
///
/// # Safety
/// `handle` must be a live streaming decoder.
#[no_mangle]
pub unsafe extern "C" fn silk_stream_sample_rate(handle: *const StreamDecoder) -> u32 {
    match handle.as_ref() {
        Some(h) => h.sample_rate,
        None => 0,
    }
}

/// Number of packets fed to this decoder so far.
///
/// # Safety
/// `handle` must be a live streaming decoder.
#[no_mangle]
pub unsafe extern "C" fn silk_stream_packets(handle: *const StreamDecoder) -> u32 {
    match handle.as_ref() {
        Some(h) => h.seen,
        None => 0,
    }
}

/// Decode one SILK packet, returning its PCM as an out-buffer of `i16`.
///
/// Returns null on failure; call [`silk_last_error`] for the reason.
///
/// # Safety
/// `handle` must be a live streaming decoder and `ptr`/`len` a readable region.
#[no_mangle]
pub unsafe extern "C" fn silk_stream_decode(
    handle: *mut StreamDecoder,
    ptr: *const u8,
    len: usize,
) -> *mut u32 {
    let Some(h) = handle.as_mut() else {
        return std::ptr::null_mut();
    };
    match h.inner.decode_packet(read_bytes(ptr, len)) {
        Ok(pcm) => {
            h.sample_rate = h.inner.sample_rate();
            h.seen += 1;
            OutBuffer::from_samples(&pcm)
        }
        Err(e) => {
            set_last_error(&e);
            std::ptr::null_mut()
        }
    }
}

/// Decode the packet starting at `offset` of a whole-file buffer.
///
/// Returns the PCM and sets `*out_advance` to the number of bytes to skip to
/// reach the next packet. This is the most convenient streaming entry point: read
/// the file once, then walk it packet by packet without re-parsing.
///
/// # Safety
/// `handle` must be live; `ptr`/`len` must be readable; `out_advance` must be valid.
#[no_mangle]
pub unsafe extern "C" fn silk_stream_decode_at(
    handle: *mut StreamDecoder,
    ptr: *const u8,
    len: usize,
    offset: u32,
    out_advance: *mut u32,
) -> *mut u32 {
    let Some(h) = handle.as_mut() else {
        return std::ptr::null_mut();
    };
    let data = read_bytes(ptr, len);
    let start = offset as usize;
    if start + 2 > data.len() {
        return std::ptr::null_mut();
    }
    let n = i16::from_le_bytes([data[start], data[start + 1]]) as i32;
    if n <= 0 || start + 2 + n as usize > data.len() {
        return std::ptr::null_mut();
    }
    let packet = &data[start + 2..start + 2 + n as usize];
    if !out_advance.is_null() {
        *out_advance = (2 + n as usize) as u32;
    }
    match h.inner.decode_packet(packet) {
        Ok(pcm) => {
            h.sample_rate = h.inner.sample_rate();
            h.seen += 1;
            OutBuffer::from_samples(&pcm)
        }
        Err(e) => {
            set_last_error(&e);
            std::ptr::null_mut()
        }
    }
}

/// Wrap raw `i16` PCM into a 16-bit mono WAV container.
///
/// # Safety
/// `ptr` must point at `len` readable `i16` values.
#[no_mangle]
pub unsafe extern "C" fn silk_wrap_wav(ptr: *const i16, len: usize, sample_rate: u32) -> *mut u32 {
    let pcm = if ptr.is_null() || len == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(ptr, len)
    };
    OutBuffer::from_bytes(&silk_decoder::wrap_wav(sample_rate, pcm))
}

/// Size in bytes of the out-buffer header (16), for host-side parsing.
#[no_mangle]
pub extern "C" fn silk_header_bytes() -> u32 {
    16
}
