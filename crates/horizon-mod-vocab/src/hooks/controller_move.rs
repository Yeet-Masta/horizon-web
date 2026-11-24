//! The `controller_move` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// Replaces gameplay left-stick shaping for this on-foot local player tick, before the stock
    /// radial dead zone. `axes` are `[right, up]` in `[-1, 1]`; `deadzone` is the configured
    /// movement dead zone. `frame_seconds` is elapsed time for the most recent advancing frame,
    /// not the tick duration.
    /// `external_server` distinguishes a multiplayer connection from an integrated/local world.
    /// Return `None` to keep the stock calculation. A centered, blocked or captured stick
    /// does not call this native-only hook. External servers still get eight-direction digital
    /// movement, and the returned direction controls which of the ordinary movement flags are set.
    ///
    /// Not on wasm: the gameplay controller transform is a trusted native hook.
    #[hook(side = "client", contract = "replace", out = "movement")]
    fn controller_move(
        axes: [f32; 2],
        deadzone: f32,
        frame_seconds: f32,
        external_server: bool,
        movement: &mut Option<ControllerMoveOverride>,
    );
}
