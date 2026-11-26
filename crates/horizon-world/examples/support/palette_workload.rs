use super::{PalettedContainer, bench_support::stats};
use horizon_core::nbt::Tag;
use serde_json::{Value, json};
use std::{hint::black_box, time::Instant};

const ENTRIES: usize = 4096;
const MIN_BITS: u8 = 4;
const EDITS: usize = 100_000;
const PACKS: usize = 1_000;

fn block_state(id: usize) -> Tag {
    Tag::compound([
        (
            "id",
            Tag::String(format!("bench:block_{}", id / 4).as_str().into()),
        ),
        (
            "properties",
            Tag::compound([
                (
                    "facing",
                    Tag::String(["north", "south", "east", "west"][id % 4].into()),
                ),
                ("waterlogged", Tag::String("false".into())),
            ]),
        ),
    ])
}

// An independent cell model for first-use order, packed words, and unused-slot removal.
fn expected_nbt(cells: &[usize], values: &[Tag]) -> Tag {
    let mut order = Vec::new();
    let ids = cells
        .iter()
        .map(|&value| {
            order
                .iter()
                .position(|&seen| seen == value)
                .unwrap_or_else(|| {
                    order.push(value);
                    order.len() - 1
                })
        })
        .collect::<Vec<_>>();
    let palette = Tag::List(order.iter().map(|&i| values[i].clone()).collect());
    if order.len() == 1 {
        return Tag::compound([("palette", palette)]);
    }
    let bits = (usize::BITS - (order.len() - 1).leading_zeros()).max(MIN_BITS.into()) as usize;
    let per_word = 64 / bits;
    let words = ids
        .chunks(per_word)
        .map(|chunk| {
            chunk
                .iter()
                .enumerate()
                .fold(0, |word, (i, &id)| word | ((id as u64) << (bits * i))) as i64
        })
        .collect();
    Tag::compound([("palette", palette), ("data", Tag::LongArray(words))])
}

fn control(container: &PalettedContainer, cells: &[usize], values: &[Tag]) {
    for (index, &id) in cells.iter().enumerate() {
        assert_eq!(container.get(index).unwrap(), &values[id]);
    }
}

fn run(p: usize, repeats: usize) -> Value {
    let values = (0..p + 1).map(block_state).collect::<Vec<_>>();
    let start = (0..ENTRIES).map(|i| p - 1 - i % p).collect::<Vec<_>>();
    let mut fixture = PalettedContainer::uniform(values[0].clone(), ENTRIES, MIN_BITS).unwrap();
    for (i, &value) in start.iter().enumerate() {
        fixture.set(i, values[value].clone()).unwrap();
    }
    assert_eq!(fixture.palette_len(), p);
    let mut cells = start;
    let mut rng = 0x1234_5678_9abc_def1u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut old = Vec::with_capacity(EDITS);
    let edits = (0..EDITS)
        .map(|_| {
            // Preserve one untouched cell for each palette entry.
            let i = p + (next() as usize % (ENTRIES - p));
            let id = next() as usize % p;
            old.push(std::mem::replace(&mut cells[i], id));
            (i, values[id].clone())
        })
        .collect::<Vec<_>>();
    let expected = expected_nbt(&cells, &values);
    let decoded = PalettedContainer::from_nbt(&expected, ENTRIES, MIN_BITS).unwrap();
    control(&decoded, &cells, &values);

    let mut set = Vec::with_capacity(repeats);
    let mut compact = Vec::with_capacity(repeats);
    let mut to_nbt = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let mut section = fixture.clone();
        let mut old_tags = Vec::with_capacity(EDITS);
        let inputs = edits.clone();
        let timer = Instant::now();
        for (i, tag) in inputs {
            old_tags.push(section.set(i, tag).unwrap());
        }
        set.push(timer.elapsed().as_secs_f64() * 1e9 / EDITS as f64);
        assert!(
            old_tags
                .iter()
                .zip(&old)
                .all(|(tag, &id)| *tag == values[id])
        );
        control(&section, &cells, &values);
        assert_eq!(section.palette_len(), p);

        // Make the correctness gate exercise a stale slot separately; the measured
        // sections below remain saturated at exactly P distinct values.
        let mut dirty = section.clone();
        dirty.set(0, values[p].clone()).unwrap();
        dirty.set(0, values[cells[0]].clone()).unwrap();
        assert_eq!(dirty.palette_len(), p + 1);
        assert_eq!(dirty.to_nbt(), expected);
        dirty.compact().unwrap();
        assert_eq!(dirty.palette_len(), p);
        assert_eq!(dirty.to_nbt(), expected);

        let mut sections = vec![section.clone(); PACKS];
        let timer = Instant::now();
        for container in &mut sections {
            black_box(container).compact().unwrap();
        }
        compact.push(timer.elapsed().as_secs_f64() * 1e9 / PACKS as f64);
        for container in &sections {
            assert_eq!(container.palette_len(), p);
            control(container, &cells, &values);
        }
        drop(sections);

        let mut saved = Vec::with_capacity(PACKS);
        let timer = Instant::now();
        for _ in 0..PACKS {
            saved.push(black_box(&section).to_nbt());
        }
        to_nbt.push(timer.elapsed().as_secs_f64() * 1e9 / PACKS as f64);
        assert!(saved.iter().all(|tag| *tag == expected));
    }
    json!({ "palette_size": p, "set": stats(&set), "compact": stats(&compact), "to_nbt": stats(&to_nbt) })
}

pub(super) fn report(json: bool, repeats: usize) {
    let rows = [1, 4, 16, 64].map(|p| run(p, repeats));
    if json {
        println!(
            "{}",
            json!({
                "benchmark": "palette",
                "unit": "ns/op",
                "section_entries": ENTRIES,
                "operations": { "set": EDITS, "compact": PACKS, "to_nbt": PACKS },
                "baselines": rows,
            })
        );
    } else {
        println!("Palette baseline (ns/op; {repeats} repeats)");
        for row in rows {
            for phase in ["set", "compact", "to_nbt"] {
                let stats = &row[phase];
                println!(
                    "P={:>2} {phase:>7}: median {:.2}, MAD {:.2}, MAD/median {:.2}%",
                    row["palette_size"],
                    stats["median"].as_f64().unwrap(),
                    stats["mad"].as_f64().unwrap(),
                    stats["mad_over_median"].as_f64().unwrap() * 100.0,
                );
            }
        }
    }
}
