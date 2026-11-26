//! Headless named block-tag baseline: `cargo run --release -p horizon-world
//! --example tag_lookup_bench -- --json --repeats 7`.
use horizon_world as world;
use serde_json::json;
use std::{hint::black_box, io, time::Instant};

#[path = "support/bench_support.rs"]
mod bench_support;
#[path = "support/tag_fixture.rs"]
mod tag_fixture;

const LOOKUPS: usize = 10_000_000;

fn main() -> io::Result<()> {
    let mut json = false;
    let mut repeats = 7;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--repeats" => {
                repeats = args
                    .next()
                    .and_then(|arg| arg.parse().ok())
                    .filter(|&n| n > 0)
                    .expect("--repeats requires a positive integer");
            }
            _ => panic!("unknown argument {arg}"),
        }
    }

    let (_, behaviours) = tag_fixture::load()?;
    let fire = behaviours.table.block_by_name("minecraft:fire").unwrap();
    let soul_fire = behaviours
        .table
        .block_by_name("minecraft:soul_fire")
        .unwrap();
    let stone = behaviours.table.block_by_name("minecraft:stone").unwrap();
    let tag = behaviours.tag("minecraft:fire").unwrap();
    assert!(tag.contains(fire) && tag.contains(soul_fire) && !tag.contains(stone));
    assert!(behaviours.tag("bench:no_such_tag").is_none());

    let mut times = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let mut hits = 0;
        let timer = Instant::now();
        for _ in 0..LOOKUPS {
            hits += usize::from(black_box(behaviours.tag(black_box("minecraft:fire"))).is_some());
        }
        times.push(timer.elapsed().as_secs_f64() * 1e9 / LOOKUPS as f64);
        assert_eq!(hits, LOOKUPS);
    }
    let stats = bench_support::stats(&times);
    if json {
        println!(
            "{}",
            json!({
                "benchmark": "tag_lookup",
                "lookup": "minecraft:fire",
                "tags": tag_fixture::TAG_COUNT,
                "lookups_per_repeat": LOOKUPS,
                "unit": "ns/lookup",
                "stats": stats,
            })
        );
    } else {
        println!(
            "minecraft:fire ({LOOKUPS} lookups per repeat): median {:.2} ns/lookup, MAD {:.2}, MAD/median {:.2}%",
            stats["median"].as_f64().unwrap(),
            stats["mad"].as_f64().unwrap(),
            stats["mad_over_median"].as_f64().unwrap() * 100.0,
        );
    }
    Ok(())
}
