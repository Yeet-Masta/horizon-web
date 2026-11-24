use super::*;

impl Game<'_> {
    /// A line the dedicated console or an RCON client is about to run (Bukkit's
    /// `ServerCommandEvent`, `RemoteServerCommandEvent`), before it is read. Set `canceled` to run
    /// nothing. Server-side.
    pub(in crate::game) fn native_console_command(
        &mut self,
        call: mod_api::__private::ConsoleCommandCall<'_>,
    ) {
        self.fire_console_command(call);
    }
}
