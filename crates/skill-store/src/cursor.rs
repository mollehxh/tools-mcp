use crate::SkillStoreError;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub(crate) fn pagination_cursor(value: &(impl Hash + ?Sized), offset: usize) -> String {
    pagination_cursor_for_fingerprint(value_fingerprint(value), offset)
}

pub(crate) fn pagination_cursor_for_fingerprint(fingerprint: u64, offset: usize) -> String {
    format!("{fingerprint:016x}:{offset}")
}

pub(crate) fn parse_pagination_cursor(
    cursor: Option<&str>,
    value: &(impl Hash + ?Sized),
    tool: &'static str,
) -> Result<usize, SkillStoreError> {
    parse_pagination_cursor_for_fingerprint(cursor, value_fingerprint(value), tool)
}

pub(crate) fn parse_pagination_cursor_for_fingerprint(
    cursor: Option<&str>,
    expected_fingerprint: u64,
    tool: &'static str,
) -> Result<usize, SkillStoreError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let invalid = || SkillStoreError::InvalidCursor { tool };
    let stale = || SkillStoreError::StaleCursor { tool };
    let (fingerprint, offset) = cursor.split_once(':').ok_or_else(invalid)?;
    let fingerprint = u64::from_str_radix(fingerprint, 16).map_err(|_| stale())?;
    if fingerprint != expected_fingerprint {
        return Err(stale());
    }
    offset.parse::<usize>().map_err(|_| invalid())
}

fn value_fingerprint(value: &(impl Hash + ?Sized)) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
