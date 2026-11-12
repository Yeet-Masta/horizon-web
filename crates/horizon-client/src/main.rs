//! Horizon: native Rust client for Minecraft Java Edition 26.3 gameplay.
#![forbid(unsafe_code)]

// The web build keeps the browser's allocator (horizon-std's web/alloc.rs).
#[cfg(not(any(feature = "system-allocator", target_family = "wasm")))]
#[global_allocator]
static ALLOCATOR: horizon_threads::GameAllocator = horizon_threads::GAME_ALLOCATOR;

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
