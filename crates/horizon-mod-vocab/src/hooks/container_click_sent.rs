//! Observation of the local client's outgoing container prediction.

pub trait Hooks {
    /// The client's `ContainerClick`, after local prediction and immediately before it is
    /// queued for the server. Not cancellable, and not a server acknowledgement. Includes
    /// slot `-999` (outside the window), the player inventory (container `0`, with no server
    /// menu or title), and unpredicted clicks (no changed slots). A click cancelled by
    /// `client_packet_send` is not queued and does not fire this hook. Does not fire on the
    /// title screen or for creative inventory actions sent as `SetCreativeModeSlot`.
    /// To render an animation, use `screen_render_background` before the contents,
    /// `screen_render_foreground` before the carried stack and tooltips, or
    /// `screen_render_post` after them. `DrawContext::gui_draw_item_with` already draws the
    /// whole stack as one item, with an optional count override.
    #[hook(side = "client", contract = "listen")]
    fn container_click_sent();
}
