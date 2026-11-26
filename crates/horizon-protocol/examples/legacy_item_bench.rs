//! Measure the 1.20.3 ItemStack cache through the public Session: clientbound slot updates
//! remember original NBT; serverbound creative-slot writes are the path that can reuse it.
//! In the repeated lane a single warmup slot update seeds each session. In the distinct lane
//! none of the creative writes have been seen by their session. Current is an identity control.
//!
//! cargo run --release --locked -p horizon-protocol --example legacy_item_bench -- --json --repeats 7
use horizon_core::nbt::Tag;
use horizon_protocol::{
    State,
    adapter::ServerVersion,
    buf::{Codec, Writer},
    ids::play::{clientbound as cb, serverbound as sb},
    item::{Component, Stack, component},
    packets::play::{ContainerSetSlot, Serverbound, SetCreativeModeSlot},
};
use sha2::{Digest, Sha256};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    error::Error,
    time::Instant,
};

#[path = "protocol_bench/bench_support.rs"]
mod bench_support;
#[path = "protocol_bench/common.rs"]
mod common;
use common::{hash_packet, play_session};

const ITEMS: usize = 32_768;
const LEGACY_STONE: i32 = 1;
const SLOT: i16 = 36;
const REPAIR_COST: i32 = 3;

struct CountingAllocator;

thread_local! {
    static ALLOCS: Cell<Option<usize>> = const { Cell::new(None) };
}

fn count_allocation() {
    let _ = ALLOCS.try_with(|count| {
        if let Some(n) = count.get() {
            count.set(Some(n + 1));
        }
    });
}

// SAFETY: All methods forward the same arguments to System. The counter's const,
// destructor-free TLS cell is accessed without allocating.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_allocation();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measure(work: impl FnOnce()) -> (f64, f64) {
    ALLOCS.set(Some(0));
    let start = Instant::now();
    work();
    let elapsed = start.elapsed();
    let allocations = ALLOCS.replace(None).unwrap();
    (
        elapsed.as_secs_f64() * 1e9 / ITEMS as f64,
        allocations as f64 / ITEMS as f64,
    )
}

struct Fixture {
    legacy_slot: Vec<u8>,
    pinned_slot: Vec<u8>,
    creative: Vec<u8>,
    original: Vec<u8>,
    reconstructed: Vec<u8>,
}

impl Fixture {
    fn new(marker: i32, pinned_stone: i32, legacy_creative_id: i32) -> Self {
        let custom = "horizon_bench_id";
        let tag = Tag::compound([
            (custom, Tag::Int(marker)),
            ("RepairCost", Tag::Int(REPAIR_COST)),
        ]);
        let write_legacy_item = |w: &mut Writer, tag: &Tag| {
            w.bool(true);
            w.varint(LEGACY_STONE);
            w.i8(1);
            w.nbt(tag);
        };
        let legacy_creative = |tag: &Tag| {
            let mut w = Writer::new();
            w.varint(legacy_creative_id);
            w.i16(SLOT);
            write_legacy_item(&mut w, tag);
            w.data
        };
        let reconstructed = legacy_creative(&tag);
        // HideFlags=0 has no component equivalent. Seeing it in a returned stack is a wire-level
        // witness of reuse, not a counter or guess based on duplicate input bytes.
        let mut original_tag = tag;
        let Tag::Compound(fields) = &mut original_tag else {
            unreachable!();
        };
        fields.insert("HideFlags".into(), Tag::Int(0));
        let mut w = Writer::new();
        w.i8(0); // the old player inventory window id
        w.varint(0);
        w.i16(SLOT);
        write_legacy_item(&mut w, &original_tag);
        let legacy_slot = w.data;

        let mut stack = Stack::new(pinned_stone, 1);
        stack.added = vec![
            (
                component::CUSTOM_DATA,
                Component::Nbt(Tag::compound([(custom, Tag::Int(marker))])),
            ),
            (component::REPAIR_COST, Component::Int(REPAIR_COST)),
        ];
        let update = ContainerSetSlot {
            container: 0,
            state: 0,
            slot: SLOT,
            item: Some(stack),
        };
        let mut w = Writer::new();
        ContainerSetSlot::write(&update, &mut w);
        Self {
            legacy_slot,
            pinned_slot: w.data,
            creative: Serverbound::SetCreativeModeSlot(SetCreativeModeSlot {
                slot: SLOT,
                item: update.item,
            })
            .encode(),
            original: legacy_creative(&original_tag),
            reconstructed,
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut repeats = 7;
    let mut json = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--repeats" => repeats = args.next().ok_or("--repeats needs a count")?.parse()?,
            "--help" | "-h" => {
                println!("usage: legacy_item_bench [--json] [--repeats N]");
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if repeats == 0 {
        return Err("--repeats must be positive".into());
    }

    let legacy = ServerVersion::V1_20_3;
    let legacy_creative_id = legacy
        .serverbound_id(State::Play, sb::SET_CREATIVE_MODE_SLOT)
        .unwrap();
    // Item 1 is minecraft:stone in 1.20.3; use the adapter's public registry map for its pinned ID.
    let pinned_stone = legacy.ids().unwrap().item.up(LEGACY_STONE).unwrap();
    let mut results = Vec::new();
    for (name, version, seeded, distinct) in [
        ("legacy_repeated", legacy, true, false),
        ("legacy_distinct", legacy, false, true),
        ("current_control", ServerVersion::Current, false, false),
    ] {
        let old = version == legacy;
        let slot_id = version
            .clientbound_id_down(State::Play, cb::CONTAINER_SET_SLOT)
            .unwrap();
        let fixtures: Vec<_> = (0..if distinct { ITEMS } else { 1 })
            .map(|i| Fixture::new(i as i32, pinned_stone, legacy_creative_id))
            .collect();
        let fixture = |i| &fixtures[if distinct { i } else { 0 }];
        let into_client = |f: &Fixture| {
            if old {
                f.legacy_slot.clone()
            } else {
                f.pinned_slot.clone()
            }
        };
        let mut timings = [const { Vec::new() }; 4];
        let mut verified = None;
        for _ in 0..repeats {
            let reader = play_session(version)?;
            let writer = play_session(version)?;
            if seeded {
                let f = fixture(0);
                for session in [&reader, &writer] {
                    let (id, payload) = session.clientbound(slot_id, into_client(f))?.unwrap();
                    assert_eq!(id, cb::CONTAINER_SET_SLOT);
                    assert_eq!(payload, f.pinned_slot);
                }
            }
            // Separate sessions keep receive-side insertions from warming the distinct send lane.
            let mut input: Vec<_> = (0..ITEMS).map(|i| into_client(fixture(i))).collect();
            let mut incoming = Vec::with_capacity(ITEMS);
            let mut outgoing = Vec::with_capacity(ITEMS);
            let (read_ns, read_allocs) = measure(|| {
                for body in input.drain(..) {
                    incoming.push(reader.clientbound(slot_id, body).unwrap().unwrap());
                }
            });
            let (write_ns, write_allocs) = measure(|| {
                for i in 0..ITEMS {
                    outgoing.push(writer.serverbound(&fixture(i).creative).unwrap().unwrap());
                }
            });
            for (series, value) in
                timings
                    .iter_mut()
                    .zip([read_ns, read_allocs, write_ns, write_allocs])
            {
                series.push(value);
            }

            let mut incoming_hash = Sha256::new();
            let mut outgoing_hash = Sha256::new();
            let mut preserved = 0;
            for (i, ((id, incoming), outgoing)) in incoming.iter().zip(&outgoing).enumerate() {
                let f = fixture(i);
                assert_eq!(*id, cb::CONTAINER_SET_SLOT);
                assert_eq!(incoming, &f.pinned_slot);
                let expected = if seeded {
                    &f.original
                } else if old {
                    &f.reconstructed
                } else {
                    &f.creative
                };
                assert_eq!(outgoing, expected);
                preserved += usize::from(old && outgoing == &f.original);
                hash_packet(&mut incoming_hash, Some(*id), incoming);
                hash_packet(&mut outgoing_hash, None, outgoing);
            }
            assert_eq!(preserved, if seeded { ITEMS } else { 0 });
            let check = (
                preserved,
                format!("{:x}", incoming_hash.finalize()),
                format!("{:x}", outgoing_hash.finalize()),
            );
            if let Some(previous) = &verified {
                assert_eq!(previous, &check, "item streams changed between samples");
            } else {
                verified = Some(check);
            }
        }
        let (preserved, incoming_sha256, outgoing_sha256) = verified.unwrap();
        let [read_ns, read_allocs, write_ns, write_allocs] = timings
            .each_ref()
            .map(|values| bench_support::stats(values));
        let result = serde_json::json!({
            "mode": name, "version": version.name(), "items_per_direction_per_sample": ITEMS,
            "wire_verified_legacy_originals_per_sample": preserved,
            "incoming_sha256": incoming_sha256, "outgoing_sha256": outgoing_sha256,
            "clientbound_ns_per_item": read_ns, "clientbound_allocations_per_item": read_allocs,
            "serverbound_ns_per_item": write_ns, "serverbound_allocations_per_item": write_allocs,
        });
        if !json {
            println!(
                "{name} ({}) items/direction={ITEMS} original NBT reused={preserved} incoming_sha256={incoming_sha256} outgoing_sha256={outgoing_sha256}",
                version.name()
            );
            for key in [
                "clientbound_ns_per_item",
                "clientbound_allocations_per_item",
                "serverbound_ns_per_item",
                "serverbound_allocations_per_item",
            ] {
                println!("  {key}: {}", result[key]);
            }
        }
        results.push(result);
    }
    if json {
        println!(
            "{}",
            serde_json::json!({"bench": "legacy_item", "repeats": repeats, "runs": results})
        );
    }
    Ok(())
}
