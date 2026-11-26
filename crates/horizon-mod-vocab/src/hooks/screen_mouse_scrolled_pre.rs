//! The `screen_mouse_scrolled_pre` hook: declarations the mod API generator reads (see `hooks/mod.rs`).
// catalog-order: 0106

pub trait Hooks {
    /// The wheel turned over the open screen (`ScreenEvent.MouseScrolled.Pre`,
    /// vanilla's `Screen.mouseScrolled`): `screen` is the screen it is,
    /// `screen.mouse` where the pointer is (so there is no position of its
    /// own), `scroll` the wheel's movement in x and y as the game's own scroll
    /// handler scales it (a line times the wheel sensitivity: with Discrete
    /// Scrolling on, a notch is ±1, its sign only; a touchpad's pixels are
    /// divided by 30), and
    /// `modifiers` GLFW's bits (shift 1, control 2, alt 4, super 8). Set
    /// `canceled` and nothing the screen would scroll moves: not the bundle
    /// under the pointer, nor a list, slider, scrollbar or map. Nothing else
    /// reads the wheel while a screen of the game's is open, so no hotbar slot
    /// changes either.
    ///
    /// It fires before the screen scrolls anything, for the window's wheel
    /// (horizon-ctl's `scroll-at` sends it through the window too), and
    /// while a screen of the game's or the title's is open (Options and the
    /// pack list opened from the title screen count, with no player and no
    /// `menu`; the title's own). It
    /// does not fire on the connect and loading screens or on server dialogs
    /// or prompts. The active book or sign editor receives it, including
    /// a wheel routed there from a controller.
    #[hook(
        side = "client",
        contract = "cancel",
        out = "canceled",
        neoforge = "ScreenEvent.MouseScrolled.Pre",
        cancellable = true
    )]
    fn screen_mouse_scrolled_pre();
}
