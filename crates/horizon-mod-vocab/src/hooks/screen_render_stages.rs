//! The `screen_render_stages` hooks and helpers: declarations the mod API generator reads (see `hooks/mod.rs`).

pub trait Hooks {
    /// Before the open screen draws itself (`ScreenEvent.Render.Pre`, vanilla's `Screen.renderWithTooltips`). Draw below it, or cancel to skip its stock drawing, Background and Foreground. The world, HUD and toasts still draw; `screen_render_post` still fires. Covers the same screens as Post, including the active book or sign editor. `screen.rect` describes the stock editor frame, container panel or whole window, as appropriate.
    #[hook(
        side = "client",
        contract = "cancel",
        out = "canceled",
        neoforge = "ScreenEvent.Render.Pre",
        cancellable = true
    )]
    fn screen_render_pre();
    /// After the open screen draws its background panel and before its widgets, items and tooltips (`ScreenEvent.Render.Background`). Screens without a panel fire it before their contents. Covers the same screens and bounds as `screen_render_post`, including the active book or sign editor. Not cancellable; does not fire if Pre cancelled.
    #[hook(
        side = "client",
        contract = "listen",
        neoforge = "ScreenEvent.Render.Background",
        cancellable = false
    )]
    fn screen_render_background();
    /// After the open screen draws its contents, before tooltips, the dragged stack and the IME overlay (`ScreenEvent.Render.Foreground`). Covers the same screens and bounds as `screen_render_post`, including the active book or sign editor. Not cancellable; does not fire if Pre cancelled.
    #[hook(
        side = "client",
        contract = "listen",
        neoforge = "ScreenEvent.Render.Foreground",
        cancellable = false
    )]
    fn screen_render_foreground();
}
