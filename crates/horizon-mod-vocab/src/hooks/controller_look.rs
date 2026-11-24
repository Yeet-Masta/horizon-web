//! The `controller_look` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// Replaces gameplay right-stick shaping for this frame, before the stock radial dead zone,
    /// curve and scope slowdown. Return `[yaw, pitch]` in degrees for this frame, right and
    /// down positive. Apply any desired inversion, sensitivity and scope scaling yourself. The
    /// game discards non-finite answers and clamps each axis to ±180 degrees. Cinematic smoothing
    /// and the ordinary look path (including other camera zoom and vehicle limits) still follow.
    /// Return `None` for stock look. A centered, blocked or captured stick does not call this
    /// native-only hook, but the cinematic camera can still ease out with a centered stick.
    ///
    /// Not on wasm: the gameplay controller transform is a trusted native hook.
    #[hook(side = "client", contract = "replace", out = "turn")]
    fn controller_look(input: &ControllerLookInput, turn: &mut Option<[f32; 2]>);
}
