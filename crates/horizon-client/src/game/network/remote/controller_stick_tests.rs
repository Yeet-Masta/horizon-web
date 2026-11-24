//! Registered native overrides drive the real client movement tick and frame look, over an
//! in-process connection. Supplying PadState directly keeps focus and timing deterministic.
use super::{connect, local_rotation_tests::finish_login, reset_player, tick, with_client_game};
use crate::game::{Game, Screen};
use horizon_client::{gamepad, input, items::ItemStack};
use horizon_protocol::{connection::Connection, packets::play::Serverbound as S};
use mod_api::{
    CameraType, ClientHost, ControllerLookEvent, ControllerLookInput, ControllerMoveEvent,
    ControllerMoveOverride, Hooks, ModEntry,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use winit::keyboard::KeyCode;

const SECONDS: f32 = 0.05;

#[derive(Clone, Copy, Default)]
enum Mode {
    #[default]
    Pass,
    Axial,
    Square,
    Invalid,
    RemapExternal,
}

#[derive(Default)]
struct Samples {
    mode: Mode,
    movement: Vec<([f32; 2], f32, f32, bool)>,
    look: Vec<ControllerLookInput>,
}

struct Native(Arc<Mutex<Samples>>);

fn axial(tilt: f32, deadzone: f32) -> f32 {
    ((tilt.abs() - deadzone).max(0.0) / (1.0 - deadzone)).copysign(tilt)
}

#[mod_api::__subscriptions]
impl Hooks for Native {
    fn controller_move(
        &self,
        _: &dyn ClientHost,
        event: &mut ControllerMoveEvent,
    ) -> Option<ControllerMoveOverride> {
        let mut samples = self.0.lock().unwrap();
        samples.movement.push((
            event.axes,
            event.deadzone,
            event.frame_seconds,
            event.external_server,
        ));
        let direction = match samples.mode {
            Mode::Axial | Mode::Square if !event.external_server => {
                let [x, y] = event.axes.map(|tilt| axial(tilt, 0.25));
                [-x, y]
            }
            Mode::Invalid => [f32::NAN, 0.0],
            Mode::RemapExternal if event.external_server => [-0.5, 0.0],
            _ => return None,
        };
        Some(ControllerMoveOverride::new(
            direction,
            !matches!(samples.mode, Mode::Square),
        ))
    }

    fn controller_look(
        &self,
        _: &dyn ClientHost,
        event: &mut ControllerLookEvent<'_>,
    ) -> Option<[f32; 2]> {
        let mut samples = self.0.lock().unwrap();
        let input = *event.input;
        samples.look.push(input);
        match samples.mode {
            Mode::Axial => {
                let [x, y] = input.axes.map(|tilt| axial(tilt, 0.34));
                let scope = if input.first_person_scoping {
                    0.125
                } else {
                    1.0
                };
                let speed = 200.0 * input.sensitivity * input.frame_seconds * scope;
                Some([
                    x * x.abs() * speed * if input.invert_x { -1.0 } else { 1.0 },
                    y * y.abs() * speed * if input.invert_y { 1.0 } else { -1.0 },
                ])
            }
            Mode::Invalid => Some([0.0, f32::INFINITY]),
            _ => None,
        }
    }
}

fn close(actual: [f32; 2], expected: [f32; 2]) {
    assert!(
        (actual[0] - expected[0]).abs() < 1.0e-5 && (actual[1] - expected[1]).abs() < 1.0e-5,
        "expected {expected:?}, got {actual:?}"
    );
}

fn look(game: &mut Game<'_>, stick: [f32; 2], seconds: f32) -> [f32; 2] {
    game.yaw = 0.0;
    game.pitch = 0.0;
    game.pad.state.right = stick;
    game.input_look(seconds);
    [game.yaw, game.pitch]
}

fn sent_inputs(game: &Game<'_>, server: &mut Connection) -> Vec<u8> {
    game.send(S::Punch(horizon_protocol::packets::Empty));
    let mut inputs = Vec::new();
    loop {
        let (id, payload) = server.recv().unwrap();
        match S::decode(id, &payload).unwrap() {
            S::PlayerInput(input) => inputs.push(input.flags),
            S::Punch(_) => return inputs,
            _ => {}
        }
    }
}

#[test]
#[ignore = "daily"]
fn native_controller_sticks_shape_real_movement_and_camera() {
    let samples = Arc::new(Mutex::new(Samples::default()));
    mod_platform::testing::with_mods(
        vec![ModEntry::new("sticks", Box::new(Native(samples.clone())))].into(),
        || {
            with_client_game(|game| {
                let mut server = Connection::local(connect(game));
                server
                    .reader
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                finish_login(&mut server);
                game.focused = true;
                game.frame_ticks = SECONDS * 20.0;
                game.settings.controller.enabled = true;
                game.settings.controller.move_deadzone = 0.18;
                game.settings.controller.look_deadzone = 0.22;
                game.settings.controller.look_sensitivity = 0.65;
                game.settings.controller.invert_x = true;
                game.settings.controller.invert_y = true;
                game.smooth_camera = false;

                // Disabled, abstaining or non-finite answers leave the stock path alone.
                let left = [0.2, 0.625];
                let right = [0.67, 0.3];
                let stock_move = gamepad::move_vector(left, 0.18).unwrap();
                let stock_look =
                    gamepad::look_delta(right, &game.settings.controller, SECONDS, false);
                assert!(mod_api::__private::set_mod_enabled("sticks", false));
                for (disabled, mode) in [
                    (true, Mode::Axial),
                    (false, Mode::Pass),
                    (false, Mode::Invalid),
                ] {
                    samples.lock().unwrap().mode = mode;
                    reset_player(game, 20);
                    game.pad.state.left = left;
                    tick(game);
                    let player = game.player.as_ref().unwrap();
                    assert_eq!(player.move_vector, stock_move);
                    assert_eq!(
                        player.applied_input,
                        input::modify_input(stock_move, None, None)
                    );
                    close(look(game, right, SECONDS), [stock_look.0, stock_look.1]);
                    if disabled {
                        let samples = samples.lock().unwrap();
                        assert!(samples.movement.is_empty() && samples.look.is_empty());
                        assert!(mod_api::__private::set_mod_enabled("sticks", true));
                    }
                }

                samples.lock().unwrap().mode = Mode::Axial;
                for (axes, expected) in [
                    ([0.2, 0.2], [0.0, 0.0]),
                    (left, [0.0, 0.5]),
                    ([0.625, 0.2], [-0.5, 0.0]),
                ] {
                    reset_player(game, 20);
                    game.yaw = 0.0;
                    game.pitch = 0.0;
                    game.pad.state.left = axes;
                    tick(game);
                    let player = game.player.as_ref().unwrap();
                    close(player.move_vector, expected);
                    close(player.applied_input, expected.map(|axis| axis * 0.98));
                    assert_eq!(player.position.x < 8.0, expected[0] < 0.0);
                    assert_eq!(player.position.z > 8.0, expected[1] > 0.0);
                    assert_eq!(
                        samples.lock().unwrap().movement.last().copied(),
                        Some((axes, 0.18, SECONDS, false))
                    );
                }

                // The opt-out belongs to one answer, never to the pad or the player.
                reset_player(game, 20);
                let axes = [0.625, 0.625];
                game.pad.state.left = axes;
                for mode in [Mode::Axial, Mode::Square, Mode::Pass] {
                    samples.lock().unwrap().mode = mode;
                    tick(game);
                    let expected = match mode {
                        Mode::Axial => [-0.49, 0.49],
                        Mode::Square => input::modify_input([-0.5, 0.5], None, None),
                        Mode::Pass => {
                            let stock = gamepad::move_vector(axes, 0.18).unwrap();
                            input::modify_input(stock, None, None)
                        }
                        _ => unreachable!(),
                    };
                    close(game.player.as_ref().unwrap().applied_input, expected);
                }
                samples.lock().unwrap().mode = Mode::Axial;

                // Full diagonal is bounded before either slowdown. The item comes from the
                // actual held stack and crouching uses the player's sneaking_speed attribute.
                let unit = std::f32::consts::FRAC_1_SQRT_2;
                for (using_item, crouching) in
                    [(false, false), (true, false), (false, true), (true, true)]
                {
                    reset_player(game, 20);
                    game.pad.state.left = [1.0, 1.0];
                    if using_item {
                        game.inventory[game.selected] = Some(ItemStack::new("minecraft:apple", 1));
                        game.using = Some((game.selected, 0));
                    }
                    if crouching {
                        game.keys.insert(KeyCode::ShiftLeft);
                        // This part of aiStep uses last tick's shift input.
                        game.player.as_mut().unwrap().shift_key_down = true;
                    }
                    tick(game);
                    let player = game.player.as_mut().unwrap();
                    assert_eq!(player.moving_slowly(), crouching);
                    assert_eq!(player.item_use_speed.is_some(), using_item);
                    let item = player.item_use_speed.unwrap_or(1.0);
                    assert!(!using_item || (0.0..1.0).contains(&item));
                    let sneak = if crouching {
                        player.attribute("minecraft:sneaking_speed").unwrap() as f32
                    } else {
                        1.0
                    };
                    close(player.move_vector, [-unit, unit]);
                    close(
                        player.applied_input,
                        [-unit, unit].map(|v| v * 0.98 * item * sneak),
                    );
                }

                reset_player(game, 20);
                close(look(game, [0.3, 0.3], SECONDS), [0.0, 0.0]);
                close(look(game, right, SECONDS), [-1.625, 0.0]);
                close(look(game, [0.3, 0.67], SECONDS), [0.0, 1.625]);
                let sample = *samples.lock().unwrap().look.last().unwrap();
                assert_eq!(sample.axes, [0.3, 0.67]);
                assert_eq!(sample.deadzone, 0.22);
                assert_eq!(sample.sensitivity, 0.65);
                assert!(sample.invert_x && sample.invert_y);
                assert!(!sample.first_person_scoping && !sample.external_server);
                assert_eq!(sample.frame_seconds, SECONDS);

                game.inventory[game.selected] = Some(ItemStack::new("minecraft:spyglass", 1));
                game.using = Some((game.selected, 0));
                game.camera_type = CameraType::FirstPerson;
                close(look(game, right, SECONDS), [-1.625 * 0.125, 0.0]);
                assert!(
                    samples
                        .lock()
                        .unwrap()
                        .look
                        .last()
                        .unwrap()
                        .first_person_scoping
                );
                game.camera_type = CameraType::ThirdPersonBack;
                close(look(game, right, 0.5), [-3.25, 0.0]);
                let sample = *samples.lock().unwrap().look.last().unwrap();
                assert!(!sample.first_person_scoping);
                assert_eq!(sample.frame_seconds, 0.1);

                // Replacement degrees go into cinematic smoothing, which still eases out at
                // center even though no replacement listener is called there.
                game.using = None;
                game.smooth_camera = true;
                let mut smoothing = gamepad::SmoothLook::default();
                let first = smoothing.turn((-1.625, 0.0), SECONDS, true);
                close(look(game, right, SECONDS), [first.0 as f32, first.1 as f32]);
                let next = smoothing.turn((0.0, 0.0), SECONDS, true);
                close(
                    look(game, [0.0; 2], SECONDS),
                    [next.0 as f32, next.1 as f32],
                );
                game.smooth_camera = false;

                // Keep the transport in process while selecting the external-session policy.
                // A mod that abstains sees stock full-speed digital motion; one that remaps can
                // change its direction flags, but cannot force analog motion or a square bypass.
                game.remote.as_mut().unwrap().external_server = true;
                let _ = sent_inputs(game, &mut server);
                for (mode, expected, flags) in [
                    (Mode::Axial, [0.0, 1.0], 1),
                    (Mode::RemapExternal, [-1.0, 0.0], 8),
                ] {
                    samples.lock().unwrap().mode = mode;
                    reset_player(game, 20);
                    game.pad.state.left = [0.0, 0.625];
                    tick(game);
                    let player = game.player.as_ref().unwrap();
                    assert_eq!(player.move_vector, expected);
                    close(player.applied_input, expected.map(|v| v * 0.98));
                    assert_eq!(game.remote.as_ref().unwrap().last_input, flags);
                    assert_eq!(sent_inputs(game, &mut server), [flags]);
                    assert!(samples.lock().unwrap().movement.last().unwrap().3);
                }
                look(game, right, SECONDS);
                assert!(samples.lock().unwrap().look.last().unwrap().external_server);
                game.remote.as_mut().unwrap().external_server = false;
                samples.lock().unwrap().mode = Mode::Axial;

                let counts = || {
                    let samples = samples.lock().unwrap();
                    (samples.movement.len(), samples.look.len())
                };
                let before = counts();
                for guard in ["disabled", "unfocused", "menu", "radial"] {
                    reset_player(game, 20);
                    game.pad.state.left = left;
                    match guard {
                        "disabled" => game.settings.controller.enabled = false,
                        "unfocused" => game.focused = false,
                        "menu" => game.screen = Screen::Inventory,
                        "radial" => {
                            game.settings.controller.radial_stick = gamepad::RadialStick::Either;
                            game.pad_radial_open();
                            assert!(game.pad_radial_captures());
                        }
                        _ => unreachable!(),
                    }
                    tick(game);
                    assert_eq!(
                        game.player.as_ref().unwrap().move_vector,
                        [0.0; 2],
                        "{guard}"
                    );
                    assert_eq!(look(game, right, SECONDS), [0.0; 2], "{guard}");
                    assert_eq!(counts(), before, "{guard}");
                    game.settings.controller.enabled = true;
                    game.focused = true;
                    game.screen = Screen::Playing;
                    game.pad.radial.cancel();
                }

                reset_player(game, 20);
                game.pad.state.left = [0.0; 2];
                game.keys.insert(KeyCode::KeyW);
                game.keys.insert(KeyCode::KeyA);
                tick(game);
                close(game.player.as_ref().unwrap().move_vector, [unit, unit]);
                close(game.player.as_ref().unwrap().applied_input, [unit, unit]);
                assert_eq!(look(game, [0.0; 2], SECONDS), [0.0; 2]);
                assert_eq!(
                    counts(),
                    before,
                    "keyboard-only input is not a stick source"
                );
            });
        },
    );
}
