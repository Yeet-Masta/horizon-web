//! Bounded diagnostics written for launcher and support tooling.
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RuntimeJar {
    pub(crate) file: String,
    pub(crate) loader: String,
    pub(crate) engine: String,
    pub(crate) woven: usize,
    pub(crate) skipped: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) first_skip_reason: Option<String>,
    pub(crate) registrations: usize,
    pub(crate) opaque: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) isolation_failure: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RuntimeReport {
    pub(crate) schema_version: u32,
    pub(crate) jars: Vec<RuntimeJar>,
}

pub(crate) fn write(home: &Path, report: RuntimeReport) -> std::io::Result<()> {
    let mut report = report;
    report.jars.truncate(256);
    let bytes = serde_json::to_vec(&report).expect("runtime report is serializable");
    if bytes.len() > MAX_BYTES {
        return Ok(());
    }
    fs::create_dir_all(home)?;
    let part = home.join(".java-runtime.json.part");
    fs::write(&part, bytes)?;
    fs::rename(part, home.join("java-runtime.json"))
}
