//! Backpressure for asynchronous world saves.
use super::*;

const MAX_ASYNC_CHUNK_WRITES: usize = 512;

impl SaveDir {
    pub(super) fn encoding_for_save_later(
        &self,
        chunks: &[Arc<Chunk>],
        registry: &'static BlockRegistry,
        biomes: &Arc<Biomes>,
    ) -> Result<Arc<Encoding>> {
        biomes.ensure_saveable()?;
        if !chunks.is_empty() && self.active_chunk_writes() >= MAX_ASYNC_CHUNK_WRITES {
            self.wait_for_chunk_writes_below(MAX_ASYNC_CHUNK_WRITES)?;
        }
        Ok(Arc::new(Encoding {
            registry,
            biomes: biomes.clone(),
            tables: self.tables.get().cloned(),
            shape: self.shape,
            section_palettes: Some(Arc::clone(&self.section_palettes)),
            block_entities: Some(Arc::clone(&self.block_entities)),
        }))
    }
}

/// Each record encoded and compressed (`None` for a deletion), in the order
/// given. Chunk compression and NBT encoding are offloaded across a persistent
/// background worker pool, keeping region file writing and header flushes
/// ordered, atomic, and thread-safe.
pub(super) fn compress_records(
    records: &[((i32, i32), &Record)],
    timestamp: u32,
) -> Vec<std::io::Result<Option<ChunkRecord>>> {
    super::compression_pool::compress_records(records, timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_std::time::Duration;

    #[test]
    fn backpressure_unblocks_below_cap_without_draining_queue() {
        static REGISTRY: OnceLock<BlockRegistry> = OnceLock::new();
        let (registry, biomes) = loot_tests::fixture();
        let registry = REGISTRY.get_or_init(|| registry);
        let biomes = Arc::new(biomes);

        let dir = horizon_std::env::temp_dir()
            .join(format!("horizon-save-backpressure-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let save = Arc::new(SaveDir::new(dir.clone(), LevelShape::OVERWORLD));

        // Hold both region locks so the writer processes one 64-chunk batch in
        // region (0, 0) when released, then blocks on region (1, 0) with the
        // remaining 448 chunk writes still queued.
        let region_a = cached_region(
            &save.regions,
            (0, 0),
            &save.region_path("region", 0, 0),
            true,
        )
        .unwrap()
        .unwrap();
        let region_b = cached_region(
            &save.regions,
            (1, 0),
            &save.region_path("region", 1, 0),
            true,
        )
        .unwrap()
        .unwrap();
        let guard_a = region_a.lock().unwrap();
        let guard_b = region_b.lock().unwrap();

        let batch_a: Vec<_> = (0..64).map(|i| ((i % 32, i / 32), None)).collect();
        save.write_records("region", batch_a, false).unwrap();
        for batch in 0..7 {
            let batch_b: Vec<_> = (0..64)
                .map(|i| ((32 + (i % 32), batch * 2 + i / 32), None))
                .collect();
            save.write_records("region", batch_b, false).unwrap();
        }
        assert_eq!(save.active_chunk_writes(), MAX_ASYNC_CHUNK_WRITES);

        let (done_tx, done_rx) = mpsc::channel();
        let save_waiter = Arc::clone(&save);
        let biomes_waiter = Arc::clone(&biomes);
        let waiter = horizon_std::thread::spawn(move || {
            let chunk = Arc::new(Chunk::empty(0, 0));
            let res = save_waiter
                .encoding_for_save_later(&[chunk], registry, &biomes_waiter)
                .map(|_| ())
                .map_err(|e| e.to_string());
            let _ = done_tx.send(res);
        });

        // Still at the cap while both regions are locked.
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        // Release only the first region: 64 writes finish, dropping active writes
        // below MAX_ASYNC_CHUNK_WRITES while 448 writes remain blocked on region_b.
        drop(guard_a);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("backpressure should unblock once active writes drop below cap")
            .unwrap();
        waiter.join().unwrap();

        assert_eq!(save.active_chunk_writes(), MAX_ASYNC_CHUNK_WRITES - 64);
        assert!(!save.writes_finished().unwrap());

        drop(guard_b);
        save.flush().unwrap();
        assert_eq!(save.active_chunk_writes(), 0);

        drop(save);
        let _ = fs::remove_dir_all(&dir);
    }
}
