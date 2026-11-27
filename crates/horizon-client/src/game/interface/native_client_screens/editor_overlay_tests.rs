use super::{
    Game, Screen,
    client_screen_tests::{logged_in, sent},
};
use crate::game::{
    books::BookScreen,
    event_loop::EventLoopCtl,
    input_trace::InputOrigin,
    native_entity_refs::tests::{run_fresh_cases, with_client_game},
};
use horizon_client::{
    containers::BlockEntity,
    hud::{HudBatch, rgba},
    items::ItemStack,
    preedit,
    signs::SignData,
    text::Text,
    toasts::SystemToast,
};
use horizon_protocol::packets::play::{self, Serverbound as S};
use horizon_std::sync::Mutex;
use mod_api::{
    ClientHost, EditorOverlayView, Flow, Hooks, ModEntry, ScreenCharTypedPreEvent,
    ScreenKeyPressedPreEvent, ScreenKeyReleasedPreEvent, ScreenMouseButtonPressedPreEvent,
    ScreenMouseButtonReleasedPreEvent, ScreenMouseDraggedPostEvent, ScreenMouseDraggedPreEvent,
    ScreenMouseScrolledPostEvent, ScreenMouseScrolledPreEvent, ScreenPreeditPreEvent,
    ScreenRenderBackgroundEvent, ScreenRenderForegroundEvent, ScreenRenderPostEvent,
    ScreenRenderPreEvent, ScreenView,
};
use std::sync::Arc;
use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{DeviceId, ElementState, Ime, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::Window,
};

const SIZE: [u32; 2] = [640, 480];
const POS: (i32, i32, i32) = (0, 65, 0);
const PAINT: u32 = rgba(35, 171, 205, 255);
const TOAST: u32 = rgba(42, 190, 220, 255);

#[derive(Default)]
struct Trace {
    events: Vec<(&'static str, &'static str, [f32; 4])>,
    snapshot: Option<EditorOverlayView>,
    replace: bool,
}

fn cancel_if(cancel: bool) -> Flow {
    if cancel { Flow::Cancel } else { Flow::Continue }
}

struct Probe(Arc<Mutex<Trace>>);

impl Probe {
    fn record(&self, stage: &'static str, screen: &ScreenView<'_>) -> bool {
        let mut trace = self.0.lock().unwrap();
        trace.events.push((stage, screen.id, screen.rect));
        trace.replace
    }
}

#[mod_api::__subscriptions]
impl Hooks for Probe {
    fn screen_render_pre(&self, _: &dyn ClientHost, event: &mut ScreenRenderPreEvent<'_>) -> Flow {
        cancel_if(self.record("pre", event.screen))
    }

    fn screen_render_background(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenRenderBackgroundEvent<'_>,
    ) {
        self.record("background", event.screen);
    }

    fn screen_render_foreground(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenRenderForegroundEvent<'_>,
    ) {
        self.record("foreground", event.screen);
    }

    fn screen_render_post(&self, host: &dyn ClientHost, event: &mut ScreenRenderPostEvent<'_>) {
        if self.record("post", event.screen) {
            let [x, y, _, _] = event.screen.rect;
            event.draw.fill([x + 1.0, y + 1.0, 2.0, 2.0], 0xFF23ABCD);
        }
        self.0.lock().unwrap().snapshot = host.editor_overlay();
    }

    fn screen_mouse_button_pressed_pre(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenMouseButtonPressedPreEvent<'_>,
    ) -> Flow {
        cancel_if(self.record("press", event.screen))
    }

    fn screen_mouse_button_released_pre(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenMouseButtonReleasedPreEvent<'_>,
    ) -> Flow {
        cancel_if(self.record("release", event.screen))
    }

    fn screen_mouse_dragged_pre(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenMouseDraggedPreEvent<'_>,
    ) -> Flow {
        cancel_if(self.record("drag", event.screen))
    }

    fn screen_mouse_scrolled_pre(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenMouseScrolledPreEvent<'_>,
    ) -> Flow {
        cancel_if(self.record("scroll", event.screen))
    }

    fn screen_mouse_dragged_post(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenMouseDraggedPostEvent<'_>,
    ) {
        self.record("drag-post", event.screen);
    }

    fn screen_mouse_scrolled_post(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenMouseScrolledPostEvent<'_>,
    ) {
        self.record("scroll-post", event.screen);
    }

    fn screen_key_pressed_pre(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenKeyPressedPreEvent<'_>,
    ) -> Flow {
        cancel_if(self.record("key", event.screen))
    }

    fn screen_key_released_pre(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenKeyReleasedPreEvent<'_>,
    ) -> Flow {
        cancel_if(self.record("key-up", event.screen))
    }

    fn screen_char_typed_pre(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenCharTypedPreEvent<'_>,
    ) -> Flow {
        self.record("char", event.screen);
        cancel_if(event.c == '!')
    }

    fn screen_preedit_pre(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenPreeditPreEvent<'_>,
    ) -> Flow {
        cancel_if(self.record("preedit", event.screen))
    }
}

struct TestLoop;
impl EventLoopCtl for TestLoop {
    fn exit(&self) {
        panic!("editor input must not close the window");
    }

    fn set_control_flow(&self, _: ControlFlow) {}
}

fn send_event(game: &mut Game<'_>, event: WindowEvent) {
    let id = game.window.as_ref().unwrap().id();
    game.handle_window_event(&TestLoop, id, event, InputOrigin::Agent);
    assert!(game.error.is_none());
}

fn move_to(game: &mut Game<'_>, x: f32, y: f32) {
    let scale = game.gui_batch().unwrap().scale;
    send_event(
        game,
        WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(f64::from(x * scale), f64::from(y * scale)),
        },
    );
}

fn button(game: &mut Game<'_>, state: ElementState) {
    send_event(
        game,
        WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state,
            button: MouseButton::Left,
        },
    );
}

fn wheel(game: &mut Game<'_>) {
    send_event(
        game,
        WindowEvent::MouseWheel {
            device_id: DeviceId::dummy(),
            delta: MouseScrollDelta::LineDelta(0.0, 1.0),
            phase: TouchPhase::Moved,
        },
    );
}

fn key(game: &mut Game<'_>, command: &str) {
    let _ = game
        .control_command(command, &std::sync::mpsc::channel().0, &TestLoop)
        .unwrap();
}

fn composition(game: &mut Game<'_>) {
    send_event(
        game,
        WindowEvent::Ime(Ime::Preedit("語".into(), Some((3, 3)))),
    );
}

fn commit_text(game: &mut Game<'_>, text: &str) {
    send_event(game, WindowEvent::Ime(Ime::Commit(text.into())));
}

fn check_events(trace: &Mutex<Trace>, id: &str, stages: &[&str]) -> [f32; 4] {
    let events = std::mem::take(&mut trace.lock().unwrap().events);
    assert_eq!(
        events.iter().map(|event| event.0).collect::<Vec<_>>(),
        stages
    );
    let rect = events[0].2;
    assert!(rect[2] > 0.0 && rect[3] > 0.0);
    assert!(
        events.iter().all(|event| event.1 == id && event.2 == rect),
        "{events:?}"
    );
    rect
}

fn frame(game: &mut Game<'_>, trace: &Mutex<Trace>, id: &str, stages: &[&str]) -> HudBatch {
    let batch = game.hud(SIZE);
    assert_eq!(
        mod_api::prelude::ScreensClientHost::observed_screen_id(game)
            .unwrap()
            .id,
        id
    );
    let view = game.screen_view_now().unwrap();
    assert_eq!((view.menu, view.title, view.slots), (None, None, 0));
    assert_eq!(view.rect, check_events(trace, id, stages));
    assert_eq!(trace.lock().unwrap().snapshot, game.editor_overlay());
    batch
}

const STAGES: &[&str] = &["pre", "background", "foreground", "post"];

#[test]
#[ignore = "daily"]
fn editor_overlays_dispatch_and_preserve_stock_interaction() {
    if run_fresh_cases(
        "game::native_client_screens::editor_overlay_tests::editor_overlays_dispatch_and_preserve_stock_interaction",
        "HORIZON_EDITOR_OVERLAY_FIXTURE",
        &["1"],
    ) {
        return;
    }
    let trace = Arc::new(Mutex::new(Trace::default()));
    let mods = vec![ModEntry::new(
        "editor-overlay",
        Box::new(Probe(trace.clone())),
    )]
    .into();
    mod_platform::testing::with_mods(mods, || {
        with_client_game(|game| {
            let mut builder = EventLoop::builder();
            #[cfg(target_os = "linux")]
            {
                use winit::platform::x11::EventLoopBuilderExtX11;
                builder.with_x11().with_any_thread(true);
            }
            let event_loop = builder.build().unwrap();
            #[allow(deprecated)]
            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_visible(false)
                            .with_inner_size(PhysicalSize::new(SIZE[0], SIZE[1])),
                    )
                    .unwrap(),
            );
            game.options.offscreen = true;
            game.renderer = Some(game.new_renderer(window.clone()).unwrap());
            game.window = Some(window);
            let mut reader = logged_in(game);
            game.remote.as_mut().unwrap().level_load.finish();
            game.title = None;
            game.screen = Screen::Playing;

            game.open_book(BookScreen::View {
                pages: ["first", "second"].map(Text::literal).into(),
                page: 0,
                lectern: None,
            });
            assert!(matches!(
                game.editor_overlay(),
                Some(EditorOverlayView::BookView {
                    page: 0,
                    page_count: 2,
                    ..
                })
            ));
            let mut toast =
                SystemToast::new(Text::literal("Still visible"), None).with_title_color(TOAST);
            let _ = toast.slide.update(0, |_| true);
            toast.slide.origin = Some(
                horizon_std::time::Instant::now() - horizon_std::time::Duration::from_millis(600),
            );
            game.narrator_toast = Some(toast);
            let stock = frame(game, &trace, "book_view", STAGES);
            let x = ((stock.width - 192.0) / 2.0).floor() + 122.0;
            trace.lock().unwrap().replace = true;
            let replaced = frame(game, &trace, "book_view", &["pre", "post"]);
            assert!(replaced.vertices.iter().any(|vertex| vertex.color == PAINT));
            assert!(replaced.vertices.iter().any(|vertex| vertex.color == TOAST));
            assert!(stock.blur_at > 0);
            assert_eq!(replaced.blur_at, stock.blur_at, "HUD geometry is preserved");
            assert!(
                replaced.vertices.len() < stock.vertices.len(),
                "the stock book is absent"
            );
            game.narrator_toast = None;

            move_to(game, x, 164.0);
            button(game, ElementState::Pressed);
            move_to(game, x + 1.0, 164.0);
            button(game, ElementState::Released);
            wheel(game);
            check_events(&trace, "book_view", &["press", "drag", "release", "scroll"]);
            assert!(matches!(
                game.editor_overlay(),
                Some(EditorOverlayView::BookView { page: 0, .. })
            ));
            trace.lock().unwrap().replace = false;
            button(game, ElementState::Pressed);
            move_to(game, x, 164.0);
            button(game, ElementState::Released);
            wheel(game);
            check_events(
                &trace,
                "book_view",
                &[
                    "press",
                    "drag",
                    "drag-post",
                    "release",
                    "scroll",
                    "scroll-post",
                ],
            );
            assert!(matches!(
                game.editor_overlay(),
                Some(EditorOverlayView::BookView { page: 1, .. })
            ));
            key(game, "key pageup tap");
            check_events(&trace, "book_view", &["key", "key-up"]);
            assert!(matches!(
                game.editor_overlay(),
                Some(EditorOverlayView::BookView { page: 0, .. })
            ));
            key(game, "key esc tap");
            check_events(&trace, "book_view", &["key"]);
            assert!(game.editor_overlay().is_none());

            assert!(game.remote_use_book(&ItemStack::new("minecraft:writable_book", 1)));
            frame(game, &trace, "book_edit", STAGES);
            commit_text(game, "A!B");
            check_events(&trace, "book_edit", &["char", "char", "char"]);
            assert!(matches!(
                game.editor_overlay(),
                Some(EditorOverlayView::BookEdit { text, selection: [2, 2], .. }) if text == "AB"
            ));
            move_to(game, stock.width / 2.0 - 50.0, 205.0);
            button(game, ElementState::Pressed);
            assert!(matches!(
                game.editor_overlay(),
                Some(EditorOverlayView::BookSign { .. })
            ));
            check_events(&trace, "book_edit", &["press"]);
            button(game, ElementState::Released);
            check_events(&trace, "book_sign", &["release"]);
            frame(game, &trace, "book_sign", STAGES);
            commit_text(game, "T!itle");
            check_events(&trace, "book_sign", &["char"; 6]);
            assert!(matches!(
                game.editor_overlay(),
                Some(EditorOverlayView::BookSign { title, .. }) if title == "Title"
            ));
            key(game, "key enter tap");
            check_events(&trace, "book_sign", &["key"]);
            assert!(game.editor_overlay().is_none());
            assert!(sent(&mut reader).into_iter().any(|packet| matches!(
                packet, S::EditBook(book)
                    if book.title.as_deref() == Some("Title") && book.pages == ["AB"]
            )));

            for (hanging, id) in [(false, "sign_edit"), (true, "hanging_sign_edit")] {
                game.world.block_entities.insert(
                    POS,
                    BlockEntity::Sign(Box::new(SignData {
                        hanging,
                        ..SignData::default()
                    })),
                );
                game.remote_open_sign_editor(play::OpenSignEditor {
                    pos: [POS.0, POS.1, POS.2],
                    front: !hanging,
                });
                frame(game, &trace, id, STAGES);
                assert!(matches!(
                    game.editor_overlay(),
                    Some(EditorOverlayView::Sign { pos, front, hanging: actual, line: 0, .. })
                        if pos == mod_api::BlockPos::new(POS.0, POS.1, POS.2)
                            && front != hanging && actual == hanging
                ));

                if hanging {
                    // A book above a sign gets the render stages and controller Escape,
                    // without sending the sign update or stealing the sign's client state.
                    game.open_book(BookScreen::View {
                        pages: Vec::new(),
                        page: 0,
                        lectern: None,
                    });
                    frame(game, &trace, "book_view", STAGES);
                    game.pad_back().unwrap();
                    check_events(&trace, "book_view", &["key"]);
                    assert!(game.sign_edit.is_some());
                }

                preedit::reset();
                trace.lock().unwrap().replace = true;
                composition(game);
                check_events(&trace, id, &["preedit"]);
                assert!(!preedit::composing());
                game.pad_back().unwrap();
                check_events(&trace, id, &["key"]);
                assert!(game.sign_edit.is_some());
                assert!(
                    !sent(&mut reader)
                        .into_iter()
                        .any(|p| matches!(p, S::SignUpdate(_)))
                );
                trace.lock().unwrap().replace = false;
                composition(game);
                check_events(&trace, id, &["preedit"]);
                assert!(preedit::composing());
                commit_text(game, "X!Y");
                check_events(&trace, id, &["char", "char", "char"]);
                key(game, "key down tap");
                check_events(&trace, id, &["key", "key-up"]);
                assert!(matches!(
                    game.editor_overlay(),
                    Some(EditorOverlayView::Sign { lines, line: 1, .. }) if lines[0] == "XY"
                ));

                game.link_prompt = Some("https://example.com".into());
                game.hud(SIZE);
                wheel(game);
                assert!(game.editor_overlay().is_none());
                assert!(trace.lock().unwrap().events.is_empty());
                game.link_prompt = None;

                game.pad_back().unwrap();
                check_events(&trace, id, &["key"]);
                assert!(game.editor_overlay().is_none());
                assert!(sent(&mut reader).into_iter().any(|packet| matches!(
                    packet, S::SignUpdate(sign)
                        if sign.pos == [POS.0, POS.1, POS.2] && sign.front != hanging && sign.lines[0] == "XY"
                )));
            }
            preedit::reset();
        });
    });
}
