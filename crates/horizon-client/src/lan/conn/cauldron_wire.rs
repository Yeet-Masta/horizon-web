//! The opt-in water-cauldron holder has an appended type ID, while its block
//! and all its states still have their original vanilla IDs. At the common
//! play egress, keep the holder only for peers that passed the content check.
//! This also covers shared saved chunks, terrain reveals and resends.
use super::Body;
use horizon_protocol::{
    Result,
    buf::{Reader, write_varint},
    ids::play::clientbound,
};

pub(super) fn project(body: Body, hidden: Option<i32>) -> Option<Body> {
    let Some(kind) = hidden else {
        return Some(body);
    };
    let id = i32::from(*body.first()?);
    match id {
        clientbound::BLOCK_ENTITY_DATA => {
            let mut reader = Reader::new(&body[1..]);
            reader.bytes(8).ok()?; // packed BlockPos
            (reader.varint().ok()? != kind).then_some(body)
        }
        clientbound::LEVEL_CHUNK_WITH_LIGHT => match without_holder(&body, kind) {
            Ok(Some(projected)) => Some(projected.into()),
            Ok(None) => Some(body),
            Err(error) => {
                log::warn!("Horizon: cannot project block entity data for a play peer: {error}");
                None
            }
        },
        _ => Some(body),
    }
}

/// Leave the section and light bytes, and every other type's record, untouched.
/// No output buffer is built for a column without the optional holder.
fn without_holder(body: &[u8], kind: i32) -> Result<Option<Vec<u8>>> {
    let mut reader = Reader::new(body);
    reader.bytes(9)?; // packet ID and chunk x/z
    for _ in 0..reader.count(16)? {
        reader.varint()?; // heightmap type
        let n = reader.count(reader.remaining() / 8)?;
        reader.bytes(n * 8)?;
    }
    let n = reader.count(reader.remaining())?;
    reader.bytes(n)?; // section buffer
    let count_at = reader.position();
    let count = reader.count(reader.remaining())?;
    let start = reader.position();
    let mut ignored = Vec::new();
    let mut ignored_bytes = 0;
    for _ in 0..count {
        let at = reader.position();
        reader.bytes(3)?; // packed x/z and short y
        let record = reader.varint()?;
        reader.nbt()?;
        if record == kind {
            let end = reader.position();
            ignored_bytes += end - at;
            ignored.push(at..end);
        }
    }
    if ignored.is_empty() {
        return Ok(None);
    }
    let mut projected = Vec::with_capacity(body.len() - ignored_bytes);
    projected.extend_from_slice(&body[..count_at]);
    write_varint(&mut projected, (count - ignored.len()) as i32);
    let mut copied = start;
    for range in ignored {
        projected.extend_from_slice(&body[copied..range.start]);
        copied = range.end;
    }
    projected.extend_from_slice(&body[copied..]);
    Ok(Some(projected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lan::{Out, PlayerSender, conn::Holding};
    use horizon_core::nbt::Tag;
    use horizon_protocol::{
        chunk::{BlockEntityInfo, ChunkData, LightData},
        packets::play::{BlockEntityData, Clientbound, LevelChunkWithLight},
    };
    use std::sync::{Arc, mpsc::channel};

    #[test]
    fn vanilla_projection_removes_only_the_opted_in_holder() {
        const MOD_TYPE: i32 = 133;
        const VANILLA_TYPE: i32 = 2;
        let sample = |kind| BlockEntityInfo {
            packed_xz: kind as i8,
            y: 55,
            kind,
            tag: Tag::compound([("sample", Tag::Int(kind))]),
        };
        let mut chunk = LevelChunkWithLight {
            x: -3,
            z: 2,
            chunk: ChunkData {
                heightmaps: vec![(0, vec![1, 2])],
                buffer: vec![0, 1, 2, 3],
                block_entities: vec![sample(MOD_TYPE), sample(VANILLA_TYPE), sample(MOD_TYPE)],
            },
            light: LightData {
                sky_mask: vec![],
                block_mask: vec![],
                empty_sky_mask: vec![7],
                empty_block_mask: vec![],
                sky: vec![],
                block: vec![],
            },
        };
        let update = |kind| {
            Clientbound::BlockEntityData(BlockEntityData {
                pos: [-44, 55, 33],
                kind,
                tag: Tag::compound([("sample", Tag::Int(kind))]),
            })
            .encode()
        };
        let holder_update = update(MOD_TYPE);
        let vanilla_update = update(VANILLA_TYPE);
        let chunk_with_holder = Clientbound::LevelChunkWithLight(chunk.clone()).encode();
        chunk.chunk.block_entities = vec![sample(VANILLA_TYPE)];
        let chunk_without_holder = Clientbound::LevelChunkWithLight(chunk).encode();

        let (tx, rx) = channel();
        let mut peer = PlayerSender::new(tx, Arc::default());
        peer.hidden_cauldron_kind = Some(MOD_TYPE);
        peer.send(holder_update.clone());
        peer.send(vanilla_update.clone());
        assert!(matches!(rx.try_recv(), Ok(Out::Packet(p)) if p == vanilla_update));
        assert!(rx.try_recv().is_err());
        let hold = Holding::start();
        peer.send_many([
            Body::shared(chunk_with_holder.clone()),
            holder_update.clone().into(),
            vanilla_update.clone().into(),
        ]);
        drop(hold);
        peer.flush();
        assert!(
            matches!(rx.try_recv(), Ok(Out::Packets(p)) if p == [chunk_without_holder, vanilla_update])
        );

        let (tx, rx) = channel();
        let matched = PlayerSender::new(tx, Arc::default());
        matched.send_many([chunk_with_holder.clone(), holder_update.clone()]);
        assert!(matches!(rx.try_recv(), Ok(Out::Packet(p)) if p == chunk_with_holder));
        assert!(matches!(rx.try_recv(), Ok(Out::Packet(p)) if p == holder_update));
    }
}
