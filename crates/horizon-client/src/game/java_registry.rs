//! Typed declarations collected while Java entrypoints bootstrap.
#![allow(dead_code)]
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct JavaRegistrySnapshot {
    pub(super) declarations: Vec<String>,
    pub(super) coalesced: BTreeMap<String, usize>,
}

impl JavaRegistrySnapshot {
    pub(super) fn from_entries(entries: &[horizon_native_java::RegistryEntry]) -> Self {
        let mut declarations = entries
            .iter()
            .map(horizon_native_java::RegistryEntry::key)
            .collect::<Vec<_>>();
        declarations.sort();
        declarations.dedup();
        Self {
            declarations,
            coalesced: BTreeMap::new(),
        }
    }
}
