//! Gives the Windows executable Horizon's icon and its stack, as horizon-client's
//! (`packaging/windows/exe.rs`).
#[path = "../../packaging/windows/exe.rs"]
mod windows_exe;

fn main() {
    windows_exe::resources("horizon-client");
    // The web build's main thread: wasm-ld's default 1 MiB stack overflowed in the release
    // build's handoff to the game (`Loading::finish`, `start_game`, `Game::new`).
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() == Ok("wasm") {
        println!("cargo:rustc-link-arg-bin=horizon-client=-zstack-size=8388608");
    }
}
