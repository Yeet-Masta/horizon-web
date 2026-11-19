use super::*;

/// The hook points. Each does nothing in Horizon.
impl Game<'_> {
    /// The client shutdown has completed.
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    pub(in crate::game) fn native_client_stopped(&mut self) {
        self.fire_client_stopped();
    }
}
