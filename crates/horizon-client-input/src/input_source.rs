//! The engine's side of the input source (docs/layers.md, "Controllers, splitscreen, replays and
//! UI pages"): what gameplay asks and tells a device beyond the keyboard and mouse. Gameplay names
//! these types, never a controller's; the controller code answers.

/// Something that happened to the player that a device may let them feel (a controller's
/// vibration). The device decides how, or whether.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Feedback {
    /// The player broke a block.
    BlockBroken,
    /// An explosion this far away (blocks) with this power.
    Explosion { distance: f64, radius: f32 },
    /// A totem of undying saved the player.
    Totem,
    /// The player split a stack into the carried one, as in Bedrock.
    ItemStackSplit,
}

/// The longest frame, in seconds, a look is turned by: a hitch's frame turns the view no further.
pub const LOOK_SECONDS_CAP: f32 = 0.1;

/// What the engine offers an input source about the game it drives: read-only, engine terms only.
/// `Game` implements it.
pub trait InputHost {
    /// A screen, a prompt or a book is open: menu navigation, not play, takes the input.
    fn menu_open(&self) -> bool;
    /// The game's window has the system's focus.
    fn focused(&self) -> bool;
    /// A player entity is in the world.
    fn has_player(&self) -> bool;
    /// The player looks through a spyglass (look is slower then).
    fn scoping(&self) -> bool;
}

/// A device beyond the keyboard and mouse that plays the game (a controller), as gameplay asks
/// it. A platform crate implements it and [`register`]s a [`Factory`]; each `Game` builds its own
/// source, so each splitscreen player has one. Every method has the answer of no device, so a
/// source implements only what it does.
pub trait InputSource {
    /// Once per frame, before the frame's input is read.
    fn frame(&mut self, _host: &dyn InputHost) {}
    /// The analog move vector (`[left, forward]`, -1..1) while the source moves the player.
    fn move_vector(&self, _host: &dyn InputHost) -> Option<[f32; 2]> {
        None
    }
    /// Whether the source holds the key mapping named `mapping` (`key.use`).
    fn holds(&self, _mapping: &str) -> bool {
        false
    }
    /// How far the source turns the view this frame, `seconds` long, in `Entity.turn`'s units
    /// (`[yaw, pitch]`, as the mouse's); `None` leaves it. The engine caps `seconds` at
    /// [`LOOK_SECONDS_CAP`], so a long frame (a hitch) doesn't spin the view.
    fn look(&mut self, _host: &dyn InputHost, _seconds: f32) -> Option<[f64; 2]> {
        None
    }
    /// The keyboard or mouse acted in play: it has control again.
    fn keyboard_used(&mut self) {}
    /// Whether the pointer event being handled is one the source made (a menu cursor clicking).
    /// The engine treats it as the pad's: it doesn't take control back for the keyboard, and the
    /// native mods' mouse hooks skip it, as they do a controller's click.
    fn made_pointer_event(&self) -> bool {
        false
    }
    /// A new player entity took over (spawn, respawn, a server joined, a dimension change).
    fn player_replaced(&mut self) {}
    /// Whether the drop about to happen is the source's whole-stack drop. Asking answers once.
    fn takes_stack_drop(&mut self) -> bool {
        false
    }
    /// The HUD starts a frame: what the source drew on the last one is gone.
    fn hud_begin(&mut self) {}
    /// Something happened to the player that the source may let them feel.
    fn feedback(&mut self, _event: Feedback) {}
}

/// Builds a game's [`InputSource`]: once per player's `Game`, so once per splitscreen player, and
/// never for the integrated server's thread or a dedicated server, which have no device.
pub trait Factory: Sync {
    fn build(&self) -> Box<dyn InputSource>;
}

static FACTORY: horizon_hooks::input::Slot<dyn Factory> = horizon_hooks::input::Slot::new();

/// Registers the factory of the game's input source, before the game runs (as
/// `horizon_hooks::register`). The first one registered is used.
pub fn register(factory: &'static dyn Factory) -> Result<(), horizon_hooks::TooLate> {
    FACTORY.register(factory)
}

/// The registered factory, `None` with none. The engine builds a source with it for each game a
/// player plays in a window, never for a server's.
pub fn factory() -> Option<&'static dyn Factory> {
    FACTORY.get()
}
