//! Headless baseline for collection and callback dispatch in `LevelTicks`.
//! All positions, priorities and due times are synthetic; no block/fluid logic runs.
mod bench_support {
    fn median(values: &mut [f64]) -> f64 {
        values.sort_unstable_by(f64::total_cmp);
        let high = values.len() / 2;
        if values.len().is_multiple_of(2) {
            values[high - 1].midpoint(values[high])
        } else {
            values[high]
        }
    }

    pub fn stats(samples: &[f64]) -> serde_json::Value {
        assert!(
            !samples.is_empty(),
            "benchmark must record at least one sample"
        );
        let mut scratch = samples.to_vec();
        let middle = median(&mut scratch);
        for (deviation, &sample) in scratch.iter_mut().zip(samples) {
            *deviation = (sample - middle).abs();
        }
        let mad = median(&mut scratch);
        let ratio = if middle != 0.0 {
            Some(mad / middle.abs())
        } else if mad == 0.0 {
            Some(0.0)
        } else {
            None
        };
        serde_json::json!({
            "samples": samples,
            "median": middle,
            "mad": mad,
            "mad_over_median": ratio,
        })
    }
}

use horizon_core::position::{BlockPos, ChunkPos};
use horizon_core::ticks::{
    ChunkTicks, LevelTicks, ScheduleResult, ScheduledTick, TickPriority, TickTypeId,
};
use serde_json::{Value, json};
use std::{error::Error, time::Instant};

const WIDTH: i32 = 100;
const CHUNKS: i32 = WIDTH * WIDTH;
const GAME_TIME: i64 = 100;
// Deliberately above the server's 65,536: collect all 500,000 dense records
// in one step instead of benchmarking the cap and deferred work.
const MAXIMUM: i32 = CHUNKS * 50 + 1;

fn chunk(index: i32) -> ChunkPos {
    ChunkPos::new(index % WIDTH - WIDTH / 2, index / WIDTH - WIDTH / 2)
}

fn loaded_level() -> LevelTicks {
    let mut level = LevelTicks::default();
    for index in 0..CHUNKS {
        level
            .add_chunk(chunk(index), ChunkTicks::default())
            .unwrap();
    }
    level
}

fn seed(level: &mut LevelTicks, density: i32) {
    for index in 0..CHUNKS {
        let chunk = chunk(index);
        for slot in 0..density {
            let tick = ScheduledTick {
                kind: TickTypeId(0),
                pos: BlockPos::new(chunk.x * 16 + slot % 16, 64, chunk.z * 16 + slot / 16),
                trigger_tick: GAME_TIME - i64::from(density - 1 - slot),
                priority: TickPriority::Normal,
                // Interleave the chunks' due work so the dense fixture exercises
                // reinserting a chunk whose new head competes with other chunks.
                sub_tick_order: i64::from(slot * CHUNKS + index),
            };
            assert_eq!(level.schedule(tick), ScheduleResult::Added);
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Begin,
    Full,
}

fn sample(levels: &mut [LevelTicks], density: i32, batches: usize, phase: Phase) -> f64 {
    let due = (CHUNKS * density) as usize;
    assert!(due < MAXIMUM as usize);
    let ready_chunks = if density == 0 { 0 } else { CHUNKS as usize };
    let mut elapsed_ns = 0.0;
    for _ in 0..batches {
        if density != 0 {
            for level in &mut *levels {
                seed(level, density);
            }
        }
        let mut ready = 0;
        let mut can_tick = |_| {
            ready += 1;
            true
        };
        match phase {
            Phase::Begin => {
                let start = Instant::now();
                for level in &mut *levels {
                    level.begin_tick(GAME_TIME, MAXIMUM, &mut can_tick).unwrap();
                }
                elapsed_ns += start.elapsed().as_secs_f64() * 1e9;
                // Dispatch and cleanup are not part of the begin_tick metric.
                for level in &mut *levels {
                    let mut collected = 0;
                    while let Some(tick) = level.next_tick().unwrap() {
                        assert_eq!(tick.sub_tick_order, collected as i64);
                        collected += 1;
                    }
                    assert_eq!(collected, due);
                    level.finish_tick().unwrap();
                }
            }
            Phase::Full => {
                let mut ran = 0;
                let start = Instant::now();
                for level in &mut *levels {
                    level
                        .tick(GAME_TIME, MAXIMUM, &mut can_tick, |_, _| ran += 1)
                        .unwrap();
                }
                elapsed_ns += start.elapsed().as_secs_f64() * 1e9;
                assert_eq!(ran, due * levels.len());
            }
        }
        assert_eq!(ready, ready_chunks * levels.len());
        if density != 0 {
            for level in &*levels {
                assert_eq!(level.count(), 0, "scheduled records left in chunk queues");
            }
        }
    }
    elapsed_ns / (batches * levels.len()) as f64
}

fn run_mode(density: i32, repeats: usize) -> Value {
    // Batch independent empty levels under each timer to amortize clock cost,
    // without charging empty begin_tick for finish_tick. For populated modes
    // each batch reseeds the same loaded level outside the timer.
    let (level_count, batches) = match density {
        0 => (32, 4_096),
        1 => (1, 16),
        _ => (1, 2),
    };
    let mut levels: Vec<_> = (0..level_count).map(|_| loaded_level()).collect();
    // An unreported warmup gives the reusable collection/dispatch buffers the
    // capacity they need. Queue trimming and any transient scheduler work still run.
    sample(&mut levels, density, 1, Phase::Full);
    let mut begin = Vec::with_capacity(repeats);
    let mut full = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        begin.push(sample(&mut levels, density, batches, Phase::Begin));
        full.push(sample(&mut levels, density, batches, Phase::Full));
    }
    json!({
        "ticks_per_chunk": density,
        "due_ticks_per_step": CHUNKS * density,
        "steps_per_sample": level_count * batches,
        "begin_tick_ns_per_step": bench_support::stats(&begin),
        "full_tick_ns_per_step": bench_support::stats(&full),
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut as_json = false;
    let mut repeats = 7;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => as_json = true,
            "--repeats" => repeats = args.next().ok_or("--repeats needs a count")?.parse()?,
            "--help" | "-h" => {
                println!("Usage: level_ticks_bench [--json] [--repeats N]");
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if repeats == 0 {
        return Err("--repeats must be positive".into());
    }

    let modes = [0, 1, 50].map(|density| run_mode(density, repeats));
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "benchmark": "level_ticks_bench",
                "loaded_chunks": CHUNKS,
                "max_ticks_per_step": MAXIMUM,
                "repeats": repeats,
                "modes": modes,
            }))?
        );
    } else {
        println!("{CHUNKS} loaded chunks, cap {MAXIMUM}, {repeats} samples per metric");
        for mode in modes {
            for (phase, key) in [
                ("begin_tick", "begin_tick_ns_per_step"),
                ("full tick", "full_tick_ns_per_step"),
            ] {
                let stats = &mode[key];
                println!(
                    "{} ticks/chunk ({} steps/sample) {phase}: median {:.2} ns/step, MAD {:.2} ns/step, MAD/median {:.2}%",
                    mode["ticks_per_chunk"],
                    mode["steps_per_sample"],
                    stats["median"].as_f64().unwrap(),
                    stats["mad"].as_f64().unwrap(),
                    stats["mad_over_median"].as_f64().unwrap() * 100.0,
                );
            }
        }
    }
    Ok(())
}
