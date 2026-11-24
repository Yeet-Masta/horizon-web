//! The visible editor owns the screen hooks without becoming a `Screen` or a server menu.
use super::{Game, Screen, books::book_rect, sign_edit::sign_editor_rect};
use horizon_client::hud::HudBatch;
use mod_api::{EditorOverlayView, HookId, ScreenView, __private::listened};
use std::borrow::Cow;
use winit::keyboard::KeyCode;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EditorOverlay {
    Book,
    Sign,
}

impl Game<'_> {
    /// The same owner for clicks, keys, IME and render hooks. Prompts and loading screens
    /// can cover both; the book is painted after the sign when both are open.
    pub(super) fn active_editor_overlay(&self) -> Option<EditorOverlay> {
        if (self.book.is_none() && self.sign_edit.is_none())
            || self.prompt_open()
            || self.title.is_some()
            || matches!(self.screen, Screen::Options | Screen::Win)
            || (self.screen == Screen::Dead && self.ui.death_confirm.is_some())
            || self.pending_entry.is_some()
            || self.integrated_starting.is_some()
            || self.pending_save.is_some()
            || self.pack_reload.is_some()
            || self.remote_screen_up()
        {
            return None;
        }
        Some(if self.book.is_some() {
            EditorOverlay::Book
        } else {
            EditorOverlay::Sign
        })
    }

    /// Keep the ordinary screen contract (no controller/focus emulation), but let a
    /// replacement editor intercept clicks and scrolling that actually target its UI.
    pub(super) fn screen_mouse_hooks_input(&self) -> bool {
        !self.pad_synthetic_input()
            || (self.active_editor_overlay().is_some() && !self.pad_keyboard_captures())
    }

    pub(in crate::game) fn editor_overlay(&self) -> Option<EditorOverlayView> {
        match self.active_editor_overlay()? {
            EditorOverlay::Book => self.book_overlay_view(),
            EditorOverlay::Sign => self.sign_overlay_view(),
        }
    }

    fn editor_screen_view(
        &self,
        batch: &HudBatch,
        editor: EditorOverlay,
    ) -> Option<ScreenView<'static>> {
        let (id, rect) = match editor {
            EditorOverlay::Book => (self.book.as_ref()?.editor_id(), book_rect(batch.width)),
            EditorOverlay::Sign => (
                self.sign_overlay_id()?,
                sign_editor_rect(batch.width, batch.height),
            ),
        };
        Some(ScreenView::from(mod_api::__private::built::ScreenView {
            id,
            menu: None,
            title: None,
            rect,
            slots: 0,
            mouse: self.cursor(),
            partial_tick: self.clock.fraction() as f32,
        }))
    }

    pub(super) fn editor_screen_view_now(
        &self,
        editor: EditorOverlay,
    ) -> Option<ScreenView<'static>> {
        self.editor_screen_view(&self.gui_batch()?, editor)
    }

    pub(super) fn draw_editor_overlays(&mut self, batch: &mut HudBatch) {
        let editor = self.active_editor_overlay();
        let view = editor
            .filter(|_| self.screen_render_listened())
            .and_then(|editor| self.editor_screen_view(batch, editor));
        let sign = view.as_ref().filter(|_| editor == Some(EditorOverlay::Sign));
        let book = view.as_ref().filter(|_| editor == Some(EditorOverlay::Book));
        if sign.is_none_or(|view| self.screen_render_begin(batch, view)) {
            self.draw_sign_editor(batch, sign);
        }
        if book.is_none_or(|view| self.screen_render_begin(batch, view)) {
            self.draw_book(batch, book);
        }
        if let Some(view) = &view {
            self.screen_render_end(batch, view);
        }
    }

    pub(super) fn editor_input(
        &mut self,
        editor: EditorOverlay,
        key: Option<KeyCode>,
        text: Option<&str>,
    ) {
        match editor {
            EditorOverlay::Book => self.book_input(key, text),
            EditorOverlay::Sign => self.sign_input(key, text),
        }
    }

    pub(super) fn editor_char_pre(&self, view: &ScreenView<'_>, c: char) -> bool {
        let mut canceled = false;
        // These fields (including the page's wrapped line count and the sign's pixel width)
        // keep their own validators. The read-only state is available separately on the host.
        self.native_screen_char_typed_pre(
            view,
            c,
            super::glfw_modifiers(self.modifiers),
            &mut None,
            &mut canceled,
        );
        canceled
    }

    /// Preserve the editor's ordinary grouped insert; only allocate if characters are removed.
    pub(super) fn editor_text_pre<'a>(&self, text: &'a str) -> Option<Cow<'a, str>> {
        if !listened(HookId::ScreenCharTypedPre) {
            return Some(Cow::Borrowed(text));
        }
        let Some(view) = self
            .active_editor_overlay()
            .and_then(|editor| self.editor_screen_view_now(editor))
        else {
            return Some(Cow::Borrowed(text));
        };
        let mut left = None;
        for (i, c) in text.char_indices() {
            if !c.is_control() && self.editor_char_pre(&view, c) {
                left.get_or_insert_with(|| text[..i].to_owned());
            } else if let Some(left) = &mut left {
                left.push(c);
            }
        }
        match left {
            Some(left) if left.is_empty() => None,
            Some(left) => Some(Cow::Owned(left)),
            None => Some(Cow::Borrowed(text)),
        }
    }
}
