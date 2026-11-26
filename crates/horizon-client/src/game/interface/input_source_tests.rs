//! Who gets an input source: each player's game its own, a server's game none.

use crate::game::execution_role::ExecutionRole;
use super::play_input::input_source_for;
use horizon_client::input_source::{Factory, InputHost, InputSource};
use horizon_hooks::input::Slot;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts what it builds; each source counts its own frames.
struct Counting(AtomicUsize);

struct Built(Rc<Cell<u32>>);

impl InputSource for Built {
    fn frame(&mut self, _host: &dyn InputHost) {
        self.0.set(self.0.get() + 1);
    }
}

thread_local! {
    static BUILT: std::cell::RefCell<Vec<Rc<Cell<u32>>>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl Factory for Counting {
    fn build(&self) -> Box<dyn InputSource> {
        self.0.fetch_add(1, Ordering::Relaxed);
        let frames = Rc::new(Cell::new(0));
        BUILT.with(|built| built.borrow_mut().push(frames.clone()));
        Box::new(Built(frames))
    }
}

struct NoHost;

impl InputHost for NoHost {
    fn menu_open(&self) -> bool {
        false
    }
    fn focused(&self) -> bool {
        true
    }
    fn has_player(&self) -> bool {
        true
    }
    fn scoping(&self) -> bool {
        false
    }
}

#[test]
fn each_player_gets_its_own_source_and_a_server_none() {
    let factory = Counting(AtomicUsize::new(0));
    assert!(input_source_for(ExecutionRole::Client, || None).is_none());
    for role in [
        ExecutionRole::IntegratedServer,
        ExecutionRole::DedicatedServer,
    ] {
        assert!(input_source_for(role, || Some(&factory)).is_none());
    }
    assert_eq!(factory.0.load(Ordering::Relaxed), 0, "a server builds none");

    let mut p1 = input_source_for(ExecutionRole::Client, || Some(&factory)).unwrap();
    let mut p2 = input_source_for(ExecutionRole::Client, || Some(&factory)).unwrap();
    assert_eq!(factory.0.load(Ordering::Relaxed), 2);
    p1.frame(&NoHost);
    p1.frame(&NoHost);
    p2.frame(&NoHost);
    let frames: Vec<u32> = BUILT.with(|built| built.borrow().iter().map(|f| f.get()).collect());
    assert_eq!(
        frames,
        [2, 1],
        "the two players' sources keep their own state"
    );
}

#[test]
fn a_servers_game_leaves_the_registry_open() {
    static SERVER_FIRST: Slot<dyn Factory> = Slot::new();
    static CLIENT_FIRST: Slot<dyn Factory> = Slot::new();
    static FACTORY: Counting = Counting(AtomicUsize::new(0));

    // A server's game built before the platform crate registers: the registration still counts.
    for role in [
        ExecutionRole::IntegratedServer,
        ExecutionRole::DedicatedServer,
    ] {
        assert!(input_source_for(role, || SERVER_FIRST.get()).is_none());
    }
    assert!(SERVER_FIRST.register(&FACTORY).is_ok(), "the slot isn't sealed");
    assert!(input_source_for(ExecutionRole::Client, || SERVER_FIRST.get()).is_some());

    // A player's game reads the slot, which seals it.
    assert!(input_source_for(ExecutionRole::Client, || CLIENT_FIRST.get()).is_none());
    assert!(CLIENT_FIRST.register(&FACTORY).is_err(), "too late after a player's game");
}
