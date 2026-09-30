//! Development CLI for the pure-Rust SILK decoder.
//!
//! Primarily used to verify the Rust port against the C reference decoder:
//!
//! ```text
//! silk-cli decode in.slk out.wav [rate]
//! silk-cli pcm    in.slk out.pcm [rate]
//! silk-cli diff   in.slk reference.pcm [rate]
//! ```

use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!("usage:");
    eprintln!("  silk-cli decode <in.slk|in.silk> <out.wav> [rate=24000]");
    eprintln!("  silk-cli pcm    <in.slk> <out.pcm> [rate=24000]");
    eprintln!("  silk-cli diff   <in.slk> <reference.pcm> [rate=24000]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        return usage();
    }
    let cmd = args[1].as_str();
    let input = &args[2];
    let output = &args[3];
    let rate: u32 = args
        .get(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(24_000);

    let data = match std::fs::read(input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let pcm = match silk_decoder::decode_silk(&data, rate) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: decode failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let secs = pcm.len() as f64 / rate as f64;
    eprintln!(
        "decoded {} samples ({:.3}s @ {} Hz) from {} bytes",
        pcm.len(),
        secs,
        rate,
        data.len()
    );

    match cmd {
        "decode" => {
            let wav = silk_decoder::wrap_wav(rate, &pcm);
            if let Err(e) = std::fs::write(output, &wav) {
                eprintln!("error: cannot write {output}: {e}");
                return ExitCode::FAILURE;
            }
            eprintln!("wrote {output} ({} bytes)", wav.len());
        }
        "pcm" => {
            let mut bytes = Vec::with_capacity(pcm.len() * 2);
            for s in &pcm {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            if let Err(e) = std::fs::write(output, &bytes) {
                eprintln!("error: cannot write {output}: {e}");
                return ExitCode::FAILURE;
            }
            eprintln!("wrote {output} ({} bytes)", bytes.len());
        }
        "diff" => {
            let reference = match std::fs::read(output) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("error: cannot read reference {output}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let mine = pcm.len() * 2;
            if reference.len() != mine {
                eprintln!(
                    "MISMATCH: length rust={} ref={} (samples {} vs {})",
                    mine,
                    reference.len(),
                    pcm.len(),
                    reference.len() / 2
                );
                return ExitCode::FAILURE;
            }
            let ref_i16: Vec<i16> = reference
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            let mut diffs = 0usize;
            let mut max_delta = 0i32;
            let mut first = None;
            for (i, (a, b)) in pcm.iter().zip(ref_i16.iter()).enumerate() {
                let d = (*a as i32 - *b as i32).abs();
                if d != 0 {
                    diffs += 1;
                    if d > max_delta {
                        max_delta = d;
                    }
                    if first.is_none() {
                        first = Some((i, *a, *b));
                    }
                }
            }
            if diffs == 0 {
                println!("IDENTICAL: {} samples match the reference exactly", pcm.len());
                return ExitCode::SUCCESS;
            }
            eprintln!(
                "DIFFERS: {diffs}/{} samples ({:.4}%) max delta {max_delta}",
                pcm.len(),
                diffs as f64 * 100.0 / pcm.len() as f64
            );
            if let Some((i, a, b)) = first {
                eprintln!("first difference at sample {i}: rust={a} ref={b}");
            }
            return ExitCode::FAILURE;
        }
        _ => return usage(),
    }

    ExitCode::SUCCESS
}
