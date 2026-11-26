use std::{
    error::Error,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    time::{Duration, Instant},
};

use horizon_core::nbt::Tag;
use horizon_protocol::{
    State,
    adapter::ServerVersion,
    buf::{Codec, Writer},
    ids::play::{clientbound as cb, serverbound as sb},
    item::Stack,
    packets::play::{ClockState, ContainerSetSlot, SetHealth, SetTime},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Record, bench_support, common};

pub(super) fn version(value: &str) -> Result<ServerVersion, Box<dyn Error>> {
    match value {
        "1.20.3" => Ok(ServerVersion::V1_20_3),
        "current" => Ok(ServerVersion::Current),
        value if value == ServerVersion::Current.name() => Ok(ServerVersion::Current),
        _ => Err(format!("unsupported replay version {value}; use 1.20.3 or current").into()),
    }
}

/// Generate the same synthetic stream in either wire layout, without using the adapter's output.
pub(super) fn make_capture(path: &Path, version: ServerVersion) -> Result<(), Box<dyn Error>> {
    let mut file = BufWriter::new(File::create(path)?);
    let mut payload = Writer::new();
    let stone = ServerVersion::V1_20_3.ids().unwrap().item.up(1).unwrap();
    for tick in 0..16_384u32 {
        payload.data.clear();
        let id = match tick % 32 {
            0 => {
                let game_time = i64::from(tick);
                let day = game_time + 6000;
                let ticking = !tick.is_multiple_of(128);
                if version == ServerVersion::Current {
                    SetTime::write(
                        &SetTime {
                            game_time,
                            clocks: vec![ClockState {
                                clock: 0,
                                total_ticks: day,
                                partial_tick: 0.0,
                                rate: if ticking { 1.0 } else { 0.0 },
                            }],
                        },
                        &mut payload,
                    );
                } else {
                    payload.i64(game_time);
                    payload.i64(if ticking { day } else { -day });
                }
                cb::SET_TIME
            }
            1 => {
                payload.i64(0x0000_bec4_0020_0000 + i64::from(tick));
                cb::KEEP_ALIVE
            }
            2 => {
                payload.i32(tick as i32);
                cb::PING
            }
            3 if tick % 128 == 3 => {
                payload.string("horizon:benchmark");
                payload.bytes(&[0x5a; 512]);
                cb::CUSTOM_PAYLOAD
            }
            4 if tick % 128 == 4 => {
                if version == ServerVersion::Current {
                    ContainerSetSlot::write(
                        &ContainerSetSlot {
                            container: 0,
                            state: 0,
                            slot: 36,
                            item: Some(Stack::new(stone, 1)),
                        },
                        &mut payload,
                    );
                } else {
                    payload.i8(0); // window
                    payload.varint(0); // state
                    payload.i16(36); // slot
                    payload.bool(true);
                    payload.varint(1); // 1.20.3 minecraft:stone
                    payload.i8(1); // count
                    payload.nbt(&Tag::End);
                }
                cb::CONTAINER_SET_SLOT
            }
            _ => {
                SetHealth::write(
                    &SetHealth {
                        health: 20.0 - (tick % 17) as f32,
                        food: 20,
                        saturation: 5.0,
                    },
                    &mut payload,
                );
                cb::SET_HEALTH
            }
        };
        let wire_id = version.clientbound_id_down(State::Play, id).unwrap();
        file.write_all(&(tick * 50).to_le_bytes())?;
        file.write_all(&wire_id.to_le_bytes())?;
        file.write_all(&(payload.data.len() as u32).to_le_bytes())?;
        file.write_all(&payload.data)?;
    }
    file.flush()?;
    Ok(())
}

/// Bot captures contain only server packets. Derive the bot's two echo replies from their
/// native wire IDs and unchanged payload layouts; timestamps carry no direction information.
fn reply(version: ServerVersion, record: &Record<'_>) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    let (id, size) = match version.clientbound_id(State::Play, record.id) {
        Some(cb::KEEP_ALIVE) => (sb::KEEP_ALIVE, 8),
        Some(cb::PING) => (sb::PONG, 4),
        _ => return Ok(None),
    };
    if record.payload.len() != size {
        return Err(format!(
            "invalid {} echo payload for packet {}",
            version.name(),
            record.id
        )
        .into());
    }
    let mut w = Writer::new();
    w.varint(id);
    w.bytes(record.payload);
    Ok(Some(w.data))
}

pub(super) fn run(
    version: ServerVersion,
    records: &[Record<'_>],
    repeats: usize,
    digest: bool,
) -> Result<Value, Box<dyn Error>> {
    let replies: Vec<_> = records
        .iter()
        .map(|record| reply(version, record))
        .collect::<Result<_, _>>()?;
    let sent = replies.iter().flatten().count();
    let count = records.len() + sent;
    // Each pass is a fresh session. Cap the number so a tiny capture cannot cause thousands of
    // expensive logins, and stage just one pass at a time to bound memory for larger captures.
    let passes = 100_000usize.div_ceil(count).clamp(1, 50);
    let mut samples = Vec::with_capacity(repeats);
    let mut verified = None;
    for _ in 0..repeats {
        let mut elapsed = Duration::ZERO;
        for _ in 0..passes {
            let session = common::play_session(version)?;
            let mut inputs: Vec<_> = records.iter().map(|p| p.payload.to_vec()).collect();
            let mut incoming = Vec::with_capacity(records.len());
            let mut outgoing = Vec::with_capacity(sent);
            let start = Instant::now();
            for ((record, payload), reply) in records.iter().zip(inputs.drain(..)).zip(&replies) {
                incoming.extend(session.clientbound(record.id, payload)?);
                while let Some(packet) = session.next_incoming() {
                    incoming.push(packet);
                }
                if let Some(reply) = reply {
                    outgoing.extend(session.serverbound(reply)?);
                    outgoing.extend(session.take_outgoing());
                }
            }
            elapsed += start.elapsed();

            if incoming.is_empty() {
                return Err(
                    "no clientbound packets emitted; check the capture's wire version".into(),
                );
            }
            if version == ServerVersion::Current {
                assert_eq!(incoming.len(), records.len());
                assert_eq!(outgoing.len(), sent);
                for ((id, payload), record) in incoming.iter().zip(records) {
                    assert_eq!(*id, record.id);
                    assert_eq!(payload.as_slice(), record.payload);
                }
                for (body, reply) in outgoing.iter().zip(replies.iter().flatten()) {
                    assert_eq!(body, reply);
                }
            }
            let hashes = digest.then(|| {
                let mut read = Sha256::new();
                for (id, body) in &incoming {
                    common::hash_packet(&mut read, Some(*id), body);
                }
                let mut write = Sha256::new();
                for body in &outgoing {
                    common::hash_packet(&mut write, None, body);
                }
                (
                    format!("{:x}", read.finalize()),
                    format!("{:x}", write.finalize()),
                )
            });
            let check = (incoming.len(), outgoing.len(), hashes);
            if let Some(previous) = &verified {
                assert_eq!(previous, &check, "replay emissions changed between passes");
            } else {
                verified = Some(check);
            }
        }
        samples.push(elapsed.as_secs_f64() * 1e9 / (count * passes) as f64);
    }
    let (incoming, outgoing, hashes) = verified.unwrap();
    let mut result = json!({
        "bench": "session_replay", "version": version.name(), "repeats": repeats,
        "passes_per_sample": passes, "clientbound_inputs_per_pass": records.len(),
        "serverbound_inputs_per_pass": sent, "incoming_outputs_per_pass": incoming,
        "outgoing_outputs_per_pass": outgoing, "ns_per_packet": bench_support::stats(&samples),
    });
    if let Some((incoming, outgoing)) = hashes {
        result["incoming_sha256"] = incoming.into();
        result["outgoing_sha256"] = outgoing.into();
    }
    Ok(result)
}
