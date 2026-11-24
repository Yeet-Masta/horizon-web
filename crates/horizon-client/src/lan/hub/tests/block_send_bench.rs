use super::{add_broadcast_test_guest, hub_with_a_guest};
use crate::lan::{Holding, Out, flush_all};
use std::hint::black_box;
use std::time::Instant;

#[test]
#[ignore = "timing"]
fn block_fanout_benchmark() {
    if std::env::var("HORIZON_BLOCK_FANOUT_BENCH").as_deref() != Ok("1") {
        eprintln!("Not measuring: explicitly set HORIZON_BLOCK_FANOUT_BENCH=1 on an idle machine.");
        return;
    }
    assert!(
        !black_box(cfg!(debug_assertions)),
        "build with --profile fast"
    );
    const SENDS: usize = 1000;
    for direct in [1usize, 2, 8, 32] {
        let body = vec![0x2a; 256];
        let mut samples = Vec::new();
        let mut allocation_samples = Vec::new();
        for _ in 0..20 {
            let (mut hub, first_rx) = hub_with_a_guest();
            let mut receivers = vec![first_rx];
            for conn in 2..=direct as u32 {
                receivers.push(add_broadcast_test_guest(&mut hub, conn));
            }
            let holders: Vec<_> = (0..direct).map(|i| (i, None)).collect();
            let holding = Holding::start();
            let (elapsed, allocations) = horizon_threads::allocation_probe::measure(|| {
                let start = Instant::now();
                for _ in 0..SENDS {
                    hub.send_to_holders(&holders, &body);
                }
                start.elapsed().as_nanos()
            });
            drop(holding);
            flush_all(hub.guests.iter().map(|g| &g.sender));
            for receiver in receivers {
                let Ok(Out::Packets(bodies)) = receiver.try_recv() else {
                    panic!("expected held fanout packets");
                };
                assert_eq!(bodies.len(), SENDS);
                assert!(bodies.iter().all(|sent| sent == &body));
                assert!(receiver.try_recv().is_err());
            }
            samples.push(elapsed / SENDS as u128);
            allocation_samples.push((allocations.calls / SENDS, allocations.bytes / SENDS));
        }
        samples.sort_unstable();
        allocation_samples.sort_unstable();
        let (calls, bytes) = allocation_samples[allocation_samples.len() / 2];
        println!(
            "BLOCK_FANOUT_BENCH {{\"direct\":{direct},\"body_bytes\":{},\"delivered_per_sample\":{},\"median_ns_per_call\":{},\"median_alloc_calls_per_call\":{calls},\"median_alloc_bytes_per_call\":{bytes}}}",
            body.len(),
            direct * SENDS,
            samples[samples.len() / 2]
        );
    }
}
