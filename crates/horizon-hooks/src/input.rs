//! Where a platform crate registers the factory of the game's input source (docs/layers.md,
//! "Controllers, splitscreen, replays and UI pages"). The source's traits live with the engine's
//! input (`horizon_client_input::input_source`), whose types this crate can't name, so the slot is
//! generic over the factory it holds; that crate keeps the one static of it.
use crate::TooLate;
use crate::registry::Registry;

/// The factory a crate registers before the game runs, read by the engine each time it builds a
/// game (one per splitscreen player). The first one registered is used.
pub struct Slot<T: ?Sized + 'static>(Registry<T>);

impl<T: ?Sized + 'static> Slot<T> {
    pub const fn new() -> Self {
        Self(Registry::new())
    }

    /// Before the game starts, as [`crate::register`].
    pub fn register(&self, factory: &'static T) -> Result<(), TooLate> {
        self.0.register(factory)
    }

    /// The factory registered first, sealing the slot on the first call. With none, one load.
    pub fn get(&'static self) -> Option<&'static T> {
        self.0.all().first().copied()
    }
}

impl<T: ?Sized + 'static> Default for Slot<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Factory: Sync {
        fn build(&self) -> u8;
    }
    struct Seven;
    impl Factory for Seven {
        fn build(&self) -> u8 {
            7
        }
    }
    struct Nine;
    impl Factory for Nine {
        fn build(&self) -> u8 {
            9
        }
    }

    #[test]
    fn the_first_factory_builds_and_the_slot_seals_when_read() {
        static EMPTY: Slot<dyn Factory> = Slot::new();
        assert!(EMPTY.get().is_none());

        static SLOT: Slot<dyn Factory> = Slot::new();
        SLOT.register(&Seven).unwrap();
        SLOT.register(&Nine).unwrap();
        assert_eq!(SLOT.get().map(|f| f.build()), Some(7));
        assert_eq!(SLOT.register(&Nine), Err(TooLate));
        assert_eq!(SLOT.get().map(|f| f.build()), Some(7));
    }
}
