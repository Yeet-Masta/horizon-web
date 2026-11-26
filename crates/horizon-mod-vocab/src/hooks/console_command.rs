//! The `console_command` hook: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// A line the dedicated console or an RCON client is about to run (Bukkit's `ServerCommandEvent`
    /// and `RemoteServerCommandEvent`), without its slash, before the command is read. `source` is
    /// `console` for the server's own input and `rcon` for an RCON client. Set `canceled` to run
    /// nothing: nothing is answered and nothing is logged by the command. Heard for every console
    /// line, vanilla's, the dedicated-only ones and a mod's own commands. Not heard for a command a
    /// mod runs itself (`run_command_with_output`) or a command block. Dedicated servers only.
    /// Server-side.
    #[hook(
        side = "server",
        contract = "cancel",
        out = "canceled",
        neoforge = "none (NeoForge's CommandEvent covers the command, not the console line)",
        paper = "ServerCommandEvent",
        cancellable = true
    )]
    fn console_command(line: &str, source: &str, canceled: &mut bool);
}
