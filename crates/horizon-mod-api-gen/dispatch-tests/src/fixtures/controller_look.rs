mod api {
    #[derive(Clone, Copy, Debug)]
    pub struct ControllerLookInput {
        pub axes: [f32; 2],
        pub deadzone: f32,
        pub sensitivity: f32,
        pub invert_x: bool,
        pub invert_y: bool,
        pub frame_seconds: f32,
        pub first_person_scoping: bool,
        pub external_server: bool,
    }
}

use mod_api::mod_api::ControllerLookInput;

#[test]
fn controller_look_can_pass_or_return_degrees() {
    struct ControllerCamera;
    #[mod_api::__subscriptions]
    impl Hooks for ControllerCamera {
        fn controller_look(
            &self,
            _: &dyn ClientHost,
            event: &mut ControllerLookEvent<'_>,
        ) -> Option<[f32; 2]> {
            let input = event.input;
            assert_eq!(input.axes, [0.5, 0.75]);
            assert_eq!(input.deadzone, 0.25);
            assert_eq!(input.sensitivity, 0.625);
            assert_eq!(input.frame_seconds, 0.0625);
            assert!(!input.invert_x && input.invert_y && input.external_server);
            input.first_person_scoping.then_some([0.5, 0.25])
        }
    }
    install(vec![ModEntry::new("look", Box::new(ControllerCamera))].into());
    for first_person_scoping in [false, true] {
        let mut turn = None;
        dispatch_controller_look(
            &TestHost,
            calls::ControllerLookCall {
                input: &ControllerLookInput {
                    axes: [0.5, 0.75],
                    deadzone: 0.25,
                    sensitivity: 0.625,
                    invert_x: false,
                    invert_y: true,
                    frame_seconds: 0.0625,
                    first_person_scoping,
                    external_server: true,
                },
                turn: &mut turn,
            },
        );
        assert_eq!(turn, first_person_scoping.then_some([0.5, 0.25]));
    }
}
