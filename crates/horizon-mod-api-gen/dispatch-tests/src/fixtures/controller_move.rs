mod api {
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct ControllerMoveOverride {
        pub direction: [f32; 2],
        pub bypass_square_movement: bool,
    }
}

use mod_api::mod_api::ControllerMoveOverride;

#[test]
fn controller_move_takes_the_first_answer_for_a_raw_sample() {
    struct ControllerMovement(Option<ControllerMoveOverride>);
    #[mod_api::__subscriptions]
    impl Hooks for ControllerMovement {
        fn controller_move(
            &self,
            _: &dyn ClientHost,
            event: &mut ControllerMoveEvent,
        ) -> Option<ControllerMoveOverride> {
            assert_eq!(event.axes, [0.125, 0.625]);
            assert_eq!(event.deadzone, 0.25);
            assert_eq!(event.frame_seconds, 0.0625);
            assert!(event.external_server);
            self.0
        }
    }
    let first = ControllerMoveOverride {
        direction: [0.0, 0.5],
        bypass_square_movement: true,
    };
    let last = ControllerMoveOverride {
        direction: [0.5, 0.0],
        bypass_square_movement: false,
    };
    install(
        vec![
            ModEntry::new("pass", Box::new(ControllerMovement(None))),
            ModEntry::new("first", Box::new(ControllerMovement(Some(first)))),
            ModEntry::new("last", Box::new(ControllerMovement(Some(last)))),
        ]
        .into(),
    );
    let mut movement = None;
    dispatch_controller_move(
        &TestHost,
        calls::ControllerMoveCall {
            axes: [0.125, 0.625],
            deadzone: 0.25,
            frame_seconds: 0.0625,
            external_server: true,
            movement: &mut movement,
        },
    );
    assert_eq!(movement, Some(first));
}
