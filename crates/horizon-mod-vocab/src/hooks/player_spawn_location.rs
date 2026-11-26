//! The `player_spawn_location` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// A player who joined is about to be placed (Paper's `AsyncPlayerSpawnLocationEvent`), after
    /// their saved position, level and rotation are read, a new player's world spawn included, and
    /// before the login packet is built. `position` is their feet, `level` their level, `yaw` and
    /// `pitch` their look. Change them to place the player there from the first frame, with no
    /// teleport after the join. A level the world does not have, or a position or look that is not
    /// finite, is ignored and the player is placed as before. Listeners run in order and each sees
    /// what the one before left. Once for each join, with no player state swapped in. Server-side.
    #[hook(
        side = "server",
        contract = "listen",
        neoforge = "none (NeoForge has no such event)",
        paper = "AsyncPlayerSpawnLocationEvent"
    )]
    fn player_spawn_location(
        player: u128,
        name: &str,
        level: &mut LevelDimension,
        position: &mut [f64; 3],
        yaw: &mut f32,
        pitch: &mut f32,
    );
}
