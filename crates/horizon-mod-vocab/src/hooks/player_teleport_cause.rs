//! The `player_teleport_cause` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// A server player was teleported from `from` to `to`, now in `dimension` (Bukkit's
    /// `PlayerTeleportEvent.getCause()`), after the move. `cause` is `command` (`/tp`, `/teleport`),
    /// `spectate` (a spectator's teleport to a player), `ender_pearl`, `chorus_fruit`,
    /// `nether_portal`, `end_portal` or `end_gateway`; a respawn is not a teleport here. Heard in
    /// addition to `player_teleported`, which keeps its published signature (#19020). Listen-only. Server-side.
    #[hook(
        side = "server",
        contract = "listen",
        neoforge = "none (EntityTeleportEvent names no cause)",
        paper = "PlayerTeleportEvent"
    )]
    fn player_teleport_cause(
        player: u128,
        name: &str,
        from: [f64; 3],
        to: [f64; 3],
        dimension: &str,
        cause: &str,
    );
}
