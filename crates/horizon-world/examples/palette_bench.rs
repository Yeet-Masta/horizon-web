//! Headless block-section palette baseline: `cargo run --release -p horizon-world
//! --example palette_bench -- --json --repeats 7`.
use horizon_world::chunk::PalettedContainer;

#[path = "support/bench_support.rs"]
mod bench_support;
#[path = "support/palette_workload.rs"]
mod palette_workload;

fn main() {
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
    palette_workload::report(json, repeats);
}
