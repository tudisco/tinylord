//! ID generation and name validation (§4.2, §4.3).

use crate::errors::ApiError;
use std::sync::OnceLock;

/// Generate a new server-side ULID (sortable, chronological). Clients never
/// supply document IDs on create (§4.3).
pub fn new_ulid() -> String {
    ulid::Ulid::new().to_string()
}

/// Current unix time in milliseconds. Central so every timestamp is consistent.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Validate a collection or database name against `^[a-zA-Z][a-zA-Z0-9_]{0,63}$`
/// (§4.2). This is the sole gate that makes it safe to interpolate the name into
/// SQL identifiers (`coll_<name>`), so it must be strict.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let first = bytes[0];
    if !first.is_ascii_alphabetic() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Validate a name and return a `validation` error if it fails.
pub fn require_valid_name(kind: &str, name: &str) -> Result<(), ApiError> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(ApiError::validation(format!(
            "invalid {kind} name; must match ^[a-zA-Z][a-zA-Z0-9_]{{0,63}}$"
        )))
    }
}

/// Regex-free validator for a JSON path of the form the query compiler builds
/// or accepts: `$`, or `$` followed by one or more `.<ident>` / `[<index>]`
/// segments (§8). Used to guard client-supplied `sort`/`projection` paths.
pub fn valid_json_path(path: &str) -> bool {
    if path == "$" {
        return true;
    }
    let Some(rest) = path.strip_prefix('$') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let mut chars = rest.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        match c {
            '.' => {
                // require at least one ident char
                let mut saw = false;
                while let Some((_, nc)) = chars.peek() {
                    if nc.is_ascii_alphanumeric() || *nc == '_' {
                        saw = true;
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !saw {
                    return false;
                }
            }
            '[' => {
                let mut saw = false;
                loop {
                    match chars.peek() {
                        Some((_, d)) if d.is_ascii_digit() => {
                            saw = true;
                            chars.next();
                        }
                        Some((_, ']')) => {
                            chars.next();
                            break;
                        }
                        _ => return false,
                    }
                }
                if !saw {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Convert a client dot-notation field key (e.g. `user.age`) into a JSON path
/// (`$.user.age`). Rejects keys with characters outside `[A-Za-z0-9_.]` or empty
/// segments so the result is always a valid, safe JSON path.
pub fn field_to_json_path(field: &str) -> Result<String, ApiError> {
    if field.is_empty() {
        return Err(ApiError::validation("empty field name in filter"));
    }
    let mut out = String::from("$");
    for seg in field.split('.') {
        if seg.is_empty() {
            return Err(ApiError::validation(format!("invalid field path: {field}")));
        }
        if !seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(ApiError::validation(format!("invalid field path: {field}")));
        }
        out.push('.');
        out.push_str(seg);
    }
    Ok(out)
}

/// Names reserved for internal per-database tables; collections may not use them.
pub fn is_reserved_collection(name: &str) -> bool {
    static RESERVED: OnceLock<Vec<&'static str>> = OnceLock::new();
    let reserved = RESERVED.get_or_init(|| vec!["changelog", "indexes"]);
    reserved.contains(&name)
}
