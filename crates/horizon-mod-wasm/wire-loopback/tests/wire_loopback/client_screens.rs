use super::*;
use mod_api::prelude::*;
use mod_api::{BlockPos, EditorOverlayView, ScreenIdentity, SignEditView, Text};
use std::cell::RefCell;

/// A client whose screen of its own opens and closes, whose inventory opens,
/// and whose sign editor is on a sign: the three results a mod reads cross the
/// wire as their native values.
#[derive(Default)]
struct Client {
    screen_open: RefCell<bool>,
    inventory: RefCell<bool>,
    sign: RefCell<Option<SignEditView>>,
    editor: RefCell<Option<EditorOverlayView>>,
    screen: Option<ScreenIdentity>,
}
impl mod_api::Host for Client {}
mod_api::impl_host_areas!(Client);
impl mod_api::ClientHost for Client {}
mod_api::impl_client_host_areas!(Client, except(screens));
impl ScreensClientHost for Client {
    fn close_screen(&mut self) -> bool {
        let open = !*self.screen_open.borrow();
        self.screen_open.replace(open);
        open
    }
    fn open_inventory(&mut self) -> bool {
        let opened = !*self.inventory.borrow();
        self.inventory.replace(opened);
        opened
    }
    fn sign_editor(&self) -> Option<SignEditView> {
        *self.sign.borrow()
    }
    fn editor_overlay(&self) -> Option<EditorOverlayView> {
        self.editor.borrow().clone()
    }
    fn current_screen(&self) -> Option<ScreenIdentity> {
        self.screen.clone()
    }
}

#[test]
fn a_wasm_mod_closes_opens_and_reads_the_sign_through_the_wire() {
    mod_api::__private::initialize().unwrap();
    let _phase = mod_api::__private::PhaseOwner::scope(mod_api::__private::ModPhase::World);
    let mut client = Client::default();
    let mut frame = boundary::Frame::default();
    frame.set_client_host_mut(&mut client);
    let mut cx = boundary::Cx::default();
    cx.frames.push(frame);
    let cx = std::cell::RefCell::new(cx);
    guest::set_transport(Some(Rc::new(move |id, bytes| {
        let mut out = Writer::new();
        // SAFETY: client and its frame remain live for this synchronous transport.
        unsafe { boundary::serve(id, &mut *cx.borrow_mut(), bytes, &mut out) }.unwrap();
        out.into_bytes()
    })));
    assert_eq!(guest::Guest.close_screen(), true);
    assert_eq!(guest::Guest.close_screen(), false);
    assert_eq!(guest::Guest.open_inventory(), true);
    assert_eq!(guest::Guest.open_inventory(), false);
    assert_eq!(guest::Guest.sign_editor(), None);
    *client.sign.borrow_mut() = Some(SignEditView {
        pos: BlockPos::new(3, 64, -2),
        front: true,
    });
    assert_eq!(
        guest::Guest.sign_editor(),
        Some(SignEditView {
            pos: BlockPos::new(3, 64, -2),
            front: true,
        })
    );
    assert_eq!(guest::Guest.editor_overlay(), None);
    for view in [
        EditorOverlayView::BookView {
            page: 1,
            page_count: 3,
            text: Some(Text::literal("a page")),
            lectern: Some(BlockPos::new(-5, 70, 12)),
            can_take: true,
        },
        EditorOverlayView::BookEdit {
            page: 0,
            page_count: 1,
            text: "Aλ".into(),
            selection: [2, 1],
        },
        EditorOverlayView::BookSign {
            title: "Title".into(),
            author: "Author".into(),
            selection: [5, 2],
        },
        EditorOverlayView::Sign {
            pos: BlockPos::new(3, 64, -2),
            front: false,
            hanging: true,
            lines: ["a", "Ω", "", ""].map(str::to_owned),
            line: 1,
            selection: [1, 1],
        },
    ] {
        *client.editor.borrow_mut() = Some(view.clone());
        assert_eq!(guest::Guest.editor_overlay(), Some(view));
    }
    guest::set_transport(None);
}

#[test]
fn a_wasm_mod_reads_the_current_screen_through_a_read_only_client_host() {
    mod_api::__private::initialize().unwrap();
    let _phase = mod_api::__private::PhaseOwner::scope(mod_api::__private::ModPhase::World);
    for (id, menu) in [
        (Some("inventory"), Some("minecraft:enchantment")),
        (Some("dialog"), None),
        (None, None),
    ] {
        let client = Client {
            screen: id.map(|id| ScreenIdentity {
                id: id.into(),
                menu: menu.map(str::to_owned),
            }),
            ..Client::default()
        };
        let mut frame = boundary::Frame::default();
        frame.set_client_host(&client);
        let mut cx = boundary::Cx::default();
        cx.frames.push(frame);
        let cx = std::cell::RefCell::new(cx);
        guest::set_transport(Some(Rc::new(move |id, bytes| {
            let mut out = Writer::new();
            // SAFETY: client and its frame remain live for this synchronous transport.
            unsafe { boundary::serve(id, &mut *cx.borrow_mut(), bytes, &mut out) }.unwrap();
            out.into_bytes()
        })));
        assert_eq!(guest::Guest.current_screen(), client.screen);
        guest::set_transport(None);
    }
}
