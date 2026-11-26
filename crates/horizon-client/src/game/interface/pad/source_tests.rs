//! A registered input source answers gameplay in the pad modules' place, and the pads act on
//! nothing meanwhile (spec #14205 step 1).

use super::super::Game;
use super::super::event_loop::EventLoopCtl;
use super::parity_tests::{pad_press, playing};
use horizon_client::actions::Action;
use horizon_client::gamepad::Button;
use horizon_client::held::Source;
use horizon_client::input_source::{InputHost, InputSource};
use horizon_client::motion::PlayerMotion;
use horizon_core::geometry::Vec3;
use std::cell::Cell;
use std::rc::Rc;
use winit::event_loop::ControlFlow;
use winit::keyboard::{KeyCode, ModifiersState};

/// What a test sets a source to answer, and what it saw. Shared with the test, since the game
/// owns the source.
#[derive(Default)]
struct Script {
    frames: Cell<u32>,
    holds: Cell<&'static [&'static str]>,
    look: Cell<Option<[f64; 2]>>,
    /// The frame time the last look was given.
    look_seconds: Cell<Option<f32>>,
    moves: Cell<Option<[f32; 2]>>,
    pointer: Cell<bool>,
    stack_drop: Cell<bool>,
}

struct Probe(Rc<Script>);

impl InputSource for Probe {
    fn frame(&mut self, _host: &dyn InputHost) {
        self.0.frames.set(self.0.frames.get() + 1);
    }
    fn move_vector(&self, _host: &dyn InputHost) -> Option<[f32; 2]> {
        self.0.moves.get()
    }
    fn holds(&self, mapping: &str) -> bool {
        self.0.holds.get().contains(&mapping)
    }
    fn look(&mut self, _host: &dyn InputHost, seconds: f32) -> Option<[f64; 2]> {
        self.0.look_seconds.set(Some(seconds));
        self.0.look.get()
    }
    fn made_pointer_event(&self) -> bool {
        self.0.pointer.get()
    }
    fn takes_stack_drop(&mut self) -> bool {
        self.0.stack_drop.replace(false)
    }
}

fn install(game: &mut Game) -> Rc<Script> {
    let script = Rc::new(Script::default());
    game.input_source = Some(Box::new(Probe(script.clone())));
    script
}

#[test]
#[ignore = "daily"]
fn a_registered_source_answers_in_the_pads_place() {
    playing(|game| {
        game.player = Some(PlayerMotion::new(game.motion, Vec3::ZERO).unwrap());
        // The pad holds use and leans its look stick, and would mark a stack drop.
        pad_press(game, Button::LeftTrigger, "key.use");
        assert!(game.input_holds("key.use"));
        game.pad.state.right = [1.0, 0.0];
        game.pad.drop_stack = true;

        let script = install(game);
        assert!(!game.input_holds("key.use"), "the source says no");
        script.holds.set(&["key.use"]);
        assert!(game.input_holds("key.use"));

        let yaw = game.yaw;
        game.input_look(0.05);
        assert_eq!(
            game.yaw, yaw,
            "no look from the source is none, not the pad's"
        );
        script.look.set(Some([100.0, 0.0]));
        game.input_look(0.05);
        assert_ne!(game.yaw, yaw, "the source's look turns the view");
        // A two-second hitch: the source is given the capped frame, as the pad's look is.
        game.input_look(2.0);
        assert_eq!(script.look_seconds.get(), Some(0.1));

        // A click the source makes is a device's, so the native mods' mouse hooks skip it.
        assert!(!game.pad_synthetic_input());
        script.pointer.set(true);
        assert!(game.pad_synthetic_input(), "the source's click isn't the mouse");
        script.pointer.set(false);

        assert!(!game.input_takes_stack_drop(), "the pad's mark isn't asked");
        script.stack_drop.set(true);
        assert!(game.input_takes_stack_drop());
        assert!(!game.input_takes_stack_drop(), "asking answers once");

        game.input_frame();
        game.input_frame();
        assert_eq!(script.frames.get(), 2);
        assert!(game.input_source.is_some(), "a frame gives the source back");
    });
}

struct Loop;

impl EventLoopCtl for Loop {
    fn exit(&self) {}
    fn set_control_flow(&self, _flow: ControlFlow) {}
}

#[test]
#[ignore = "daily"]
fn the_pad_acts_on_nothing_while_a_source_drives() {
    playing(|game| {
        game.pad.inject_button(Button::South, true);
        game.poll_pads(&Loop);
        assert!(game.keys.held(Action::Jump), "with no source the pad jumps");
    });
    playing(|game| {
        install(game);
        game.pad.inject_button(Button::South, true);
        game.poll_pads(&Loop);
        assert!(
            !game.keys.held(Action::Jump),
            "the source drives, not the pad"
        );
    });
}

#[test]
#[ignore = "daily"]
fn the_keyboard_letting_go_keeps_the_sneak_a_source_holds() {
    // The pad's Toggle Sneak is the built-in pad's setting: either way, a source's hold counts.
    for pad_toggle_sneak in [false, true] {
        playing(|game| {
            // Hold mode: Shift's release would end the sneak.
            game.settings.toggle_crouch = false;
            game.settings.controller.toggle_sneak = pad_toggle_sneak;
            let script = install(game);
            script.holds.set(&["key.sneak"]);
            let shift = KeyCode::ShiftLeft;
            game.mapped_key(shift, true, false, shift, Source::Keyboard)
                .unwrap();
            game.mapped_key(shift, false, false, shift, Source::Keyboard)
                .unwrap();
            assert!(
                game.keys.held(Action::Sneak),
                "the source still holds it (pad toggle sneak {pad_toggle_sneak})"
            );
            // The window then reports Shift up.
            game.update_modifiers(ModifiersState::empty());
            assert!(
                game.keys.held(Action::Sneak),
                "the modifier going up too (pad toggle sneak {pad_toggle_sneak})"
            );
        });
    }
}

#[test]
#[ignore = "daily"]
fn a_scripted_run_leaves_the_source_unread() {
    playing(|game| {
        game.player = Some(PlayerMotion::new(game.motion, Vec3::ZERO).unwrap());
        let script = install(game);
        script.moves.set(Some([0.0, 1.0]));
        script.look.set(Some([100.0, 0.0]));
        script.holds.set(&["key.use"]);
        script.pointer.set(true);
        script.stack_drop.set(true);
        game.tick_script.play_empty();
        assert!(game.scripted_run());

        assert!(!game.input_holds("key.use"), "no hold from the source");
        assert!(!game.input_made_pointer_event(), "no click from the source");
        assert!(!game.input_takes_stack_drop(), "no stack drop from the source");

        assert_eq!(game.input_move(), None, "no move from the source");
        let yaw = game.yaw;
        game.input_look(0.05);
        assert_eq!(game.yaw, yaw, "no turn from the source");
        game.input_frame();
        assert_eq!(script.frames.get(), 0, "the source isn't driven");
        assert!(game.input_source.is_some(), "it stays registered");
    });
}
