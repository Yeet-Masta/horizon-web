//! Replay a horizon-bots --capture as 50 identical connections for compression, or replay
//! its Play records through the protocol Session. Neither mode opens a network connection.
//!
//! Native-wire captures use the same framing for both versions; actual horizon-bots captures
//! record clientbound packets *after* any adaptation, so replay those with `--session current`.
//!
//! Session replay consumes one Play segment in record order and derives keepalive/pong replies;
//! the capture has no client packets. `--digest` hashes the emitted streams for one pass,
//! including queued packets but excluding setup and capture timestamps.
//! To exercise 1.20.3 with no server, generate its native layout and a matching current control:
//!
//! ```text
//! compression_bench --make-capture 1.20.3 /tmp/legacy.cap
//! compression_bench --make-capture current /tmp/current.cap
//! compression_bench --session 1.20.3 --digest --json --repeats 7 /tmp/legacy.cap
//! compression_bench --session current --digest --json --repeats 7 /tmp/current.cap
//! ```
use flate2::{Compress, Compression, FlushCompress, Status};
use horizon_protocol::{adapter::ServerVersion, buf::write_varint};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{error::Error, path::Path, time::Instant};

#[path = "protocol_bench/bench_support.rs"]
mod bench_support;
#[path = "protocol_bench/common.rs"]
mod common;
#[path = "protocol_bench/replay.rs"]
mod replay;

const CONNECTIONS: usize = 50;
const USAGE: &str = "usage: compression_bench [--json] [--repeats N] CAPTURE\n  \
                    compression_bench --session {1.20.3|current} [--digest] [--json] [--repeats N] CAPTURE\n  \
                    compression_bench --make-capture {1.20.3|current} CAPTURE";

enum Mode {
    Compression,
    Replay(ServerVersion),
    Generate(ServerVersion),
}

struct Record<'a> {
    id: i32,
    payload: &'a [u8],
}

fn capture_records(mut data: &[u8]) -> Vec<Record<'_>> {
    let mut records = Vec::new();
    while let Some((header, rest)) = data.split_at_checked(12) {
        // LE: u32 elapsed milliseconds, i32 packet ID, u32 payload length. No direction bit.
        let id = i32::from_le_bytes(header[4..8].try_into().unwrap());
        let len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        // A capture can end on a partial record if the bot was interrupted.
        let Some((payload, next)) = rest.split_at_checked(len) else {
            break;
        };
        records.push(Record { id, payload });
        data = next;
    }
    records
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut path = None;
    let mut mode = Mode::Compression;
    let mut json = false;
    let mut digest = false;
    let mut repeats = 7;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--json" => json = true,
            "--digest" => digest = true,
            "--repeats" => repeats = args.next().ok_or("--repeats needs a count")?.parse()?,
            "--session" | "--make-capture" => {
                let version = replay::version(&args.next().ok_or("missing wire version")?)?;
                if !matches!(mode, Mode::Compression) {
                    return Err(USAGE.into());
                }
                mode = if arg == "--make-capture" {
                    Mode::Generate(version)
                } else {
                    Mode::Replay(version)
                };
            }
            _ if !arg.starts_with('-') && path.is_none() => path = Some(arg),
            _ => return Err(USAGE.into()),
        }
    }
    let path = path.ok_or(USAGE)?;
    if repeats == 0 {
        return Err("--repeats must be positive".into());
    }
    if digest && !matches!(mode, Mode::Replay(_)) {
        return Err("--digest requires --session".into());
    }
    if let Mode::Generate(version) = mode {
        return replay::make_capture(Path::new(&path), version);
    }
    let data = std::fs::read(path)?;
    let records = capture_records(&data);
    if records.is_empty() {
        return Err("empty capture".into());
    }
    if let Mode::Replay(version) = mode {
        let mut result = replay::run(version, &records, repeats, digest)?;
        result["capture_sha256"] = format!("{:x}", Sha256::digest(&data)).into();
        if json {
            println!("{result}");
        } else {
            println!(
                "version={} inputs/pass={}/{} passes/sample={} capture_sha256={}",
                version.name(),
                result["clientbound_inputs_per_pass"],
                result["serverbound_inputs_per_pass"],
                result["passes_per_sample"],
                result["capture_sha256"],
            );
            println!("  ns/packet: {}", result["ns_per_packet"]);
            if digest {
                println!(
                    "  incoming_sha256={} outgoing_sha256={}",
                    result["incoming_sha256"], result["outgoing_sha256"],
                );
            }
        }
        return Ok(());
    }
    compression(&records, repeats, json)
}

fn compression(records: &[Record<'_>], repeats: usize, json: bool) -> Result<(), Box<dyn Error>> {
    let packets: Vec<_> = records
        .iter()
        .map(|record| {
            let mut body = Vec::with_capacity(record.payload.len() + 5);
            write_varint(&mut body, record.id);
            body.extend_from_slice(record.payload);
            body
        })
        .collect();
    let mut results = Vec::new();
    for level in [6, 2] {
        let mut samples = Vec::with_capacity(repeats);
        let mut verified_bytes = None;
        for _ in 0..repeats {
            let mut z = Compress::new(Compression::new(level), true);
            let start = Instant::now();
            let bytes = compress_capture(&packets, &mut z)?;
            samples.push(start.elapsed().as_secs_f64());
            assert_eq!(bytes, *verified_bytes.get_or_insert(bytes));
        }
        let bytes = verified_bytes.unwrap();
        let seconds = bench_support::stats(&samples);
        if !json {
            println!(
                "level={level} packets={} connections={CONNECTIONS} bytes={bytes} seconds={:.3}",
                packets.len(),
                seconds["median"].as_f64().unwrap(),
            );
            println!("  seconds: {seconds}");
        }
        results.push(json!({ "level": level, "bytes": bytes, "seconds": seconds }));
    }
    if json {
        println!(
            "{}",
            json!({
                "bench": "compression", "repeats": repeats, "connections": CONNECTIONS,
                "packets_per_connection": packets.len(), "runs": results,
            })
        );
    }
    Ok(())
}

fn compress_capture(packets: &[Vec<u8>], z: &mut Compress) -> Result<usize, Box<dyn Error>> {
    let mut bytes = 0;
    for _ in 0..CONNECTIONS {
        for body in packets {
            let mut out = Vec::with_capacity(body.len() + (body.len() >> 10) + 72);
            if body.len() < 256 {
                out.push(0);
                out.extend_from_slice(body);
            } else {
                write_varint(&mut out, body.len() as i32);
                z.reset();
                let begin = z.total_in();
                let mut flush = FlushCompress::None;
                loop {
                    let offset = (z.total_in() - begin) as usize;
                    if offset == body.len() {
                        flush = FlushCompress::Finish;
                    }
                    if out.capacity() - out.len() < 64 {
                        out.reserve(32 << 10);
                    }
                    if z.compress_vec(&body[offset..], &mut out, flush)? == Status::StreamEnd {
                        break;
                    }
                }
            }
            let mut prefix = Vec::new();
            write_varint(&mut prefix, out.len() as i32);
            bytes += prefix.len() + out.len();
        }
    }
    Ok(bytes)
}
