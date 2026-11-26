//! The `player_game_mode_cause` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// A server player's game mode changed from `from` to `to` (Bukkit's
    /// `PlayerGameModeChangeEvent.getCause()`), heard after `player_game_mode_changed` has let it
    /// through and the new mode is applied. `cause` is `command` (`/gamemode`), `switcher` (the
    /// F3+F4 menu), `plugin` (a mod's own `set_game_mode`), `hardcore_death` (a hardcore world's
    /// respawn into spectator) or `unknown`. Listen-only; a separate hook, because
    /// `player_game_mode_changed` keeps its published signature (#19020). Server-side.
    #[hook(
        side = "server",
        contract = "listen",
        neoforge = "none (NeoForge has no cause)",
        paper = "PlayerGameModeChangeEvent"
    )]
    fn player_game_mode_cause(player: u128, name: &str, from: GameMode, to: GameMode, cause: &str);
}
