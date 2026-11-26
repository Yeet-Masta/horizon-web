// hidden: engine tick loop queries for per-entity tick ordering, not part of the mod surface.
pub mod __private {
    pub use crate::parts::{entity_tick_each_all, entity_tick_each_any, entity_tick_each_for};
}
