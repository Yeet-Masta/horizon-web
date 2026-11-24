//! Resource identifiers (`Identifier`).

#[inline]
fn is_ns_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
}

#[inline]
fn is_path_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-' | b'/')
}

#[inline]
fn with_default_namespace(path: &str) -> String {
    const PREFIX: &str = "minecraft:";
    let mut out = String::with_capacity(PREFIX.len() + path.len());
    out.push_str(PREFIX);
    out.push_str(path);
    out
}

/// Identifier.parse: `ns:path` or `path`, validated.
pub fn parse_identifier(raw: &str) -> Option<String> {
    match raw.split_once(':') {
        Some((ns, path)) if !ns.is_empty() => {
            if ns != ".." && ns.bytes().all(is_ns_byte) && path.bytes().all(is_path_byte) {
                Some(raw.to_owned())
            } else {
                None
            }
        }
        Some(("", path)) => path
            .bytes()
            .all(is_path_byte)
            .then(|| with_default_namespace(path)),
        None => raw
            .bytes()
            .all(is_path_byte)
            .then(|| with_default_namespace(raw)),
        Some(_) => unreachable!(),
    }
}

/// `Identifier.CODEC` parsing an entity id: lowercase namespace and path
/// characters, `minecraft` when the namespace is left out.
pub fn normalize_id(s: &str) -> Option<String> {
    match s.split_once(':') {
        Some((ns, path)) if !ns.is_empty() => {
            if ns.bytes().all(is_ns_byte) && path.bytes().all(is_path_byte) {
                Some(s.to_owned())
            } else {
                None
            }
        }
        Some(("", path)) => path
            .bytes()
            .all(is_path_byte)
            .then(|| with_default_namespace(path)),
        None => s
            .bytes()
            .all(is_path_byte)
            .then(|| with_default_namespace(s)),
        Some(_) => unreachable!(),
    }
}
