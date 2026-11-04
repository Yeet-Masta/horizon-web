//! Horizon: native Rust client for Minecraft Java Edition 26.3 gameplay.
#![forbid(unsafe_code)]

// The web build keeps the browser's allocator (horizon-platform's web/alloc.rs).
#[cfg(not(any(
    feature = "system-allocator",
    feature = "alloc-census",
    target_family = "wasm"
)))]
#[global_allocator]
static ALLOCATOR: horizon_threads::Allocator = horizon_threads::ALLOCATOR;
#[cfg(all(
    feature = "alloc-census",
    not(feature = "system-allocator"),
    not(target_family = "wasm")
))]
#[global_allocator]
static ALLOCATOR: horizon_threads::CensusAllocator = horizon_threads::CensusAllocator;

/// The mod API level this game serves, kept in the program for the launcher
/// to read without running it (`horizon_mods::api_level::MARK`). `main`
/// reads it, so the linker keeps it.
#[cfg(not(target_family = "wasm"))]
static MOD_API_LEVEL: [u8; 27] = horizon_mods::api_level::MARK;

#[cfg(not(target_family = "wasm"))]
fn main() {
    std::hint::black_box(&MOD_API_LEVEL);
    horizon_client::entry::main();
}

/// The web build: the page's start runs the game (`game::web`).
#[cfg(target_family = "wasm")]
fn main() {
    horizon_client::game::web::start();
}
