use crate::SkillStoreError;
use crate::contracts::{SkillReadOutput, SkillScope};
use crate::cursor::{pagination_cursor_for_fingerprint, parse_pagination_cursor_for_fingerprint};
use crate::roots::is_portable_segment;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

pub(crate) const MAX_HANDLE_BYTES: usize = 2_048;
pub(crate) const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const STREAM_READ_BYTES: usize = 64 * 1024;

pub(crate) fn validate_handle(field: &'static str, value: &str) -> Result<(), SkillStoreError> {
    if !value.is_empty() && value.len() <= MAX_HANDLE_BYTES && !value.chars().any(char::is_control)
    {
        return Ok(());
    }
    Err(SkillStoreError::InvalidHandle { field })
}

pub(crate) fn relative_resource_path(
    scope: SkillScope,
    package: &str,
    resource: &str,
) -> Result<PathBuf, SkillStoreError> {
    validate_handle("package", package)?;
    validate_handle("resource", resource)?;
    if !is_portable_segment(package) {
        return Err(SkillStoreError::InvalidResource);
    }
    let prefix = format!("skill://host/{}/{package}/", scope.as_str());
    let relative = resource
        .strip_prefix(&prefix)
        .ok_or(SkillStoreError::InvalidResource)?;
    let mut path = PathBuf::new();
    let mut saw_component = false;
    for component in relative.split('/') {
        if !is_portable_segment(component) {
            return Err(SkillStoreError::InvalidResource);
        }
        saw_component = true;
        path.push(component);
    }
    saw_component
        .then_some(path)
        .ok_or(SkillStoreError::InvalidResource)
}

pub(crate) fn page_response_from_reader(
    resource: &str,
    reader: &mut (impl Read + Seek),
    cursor: Option<&str>,
) -> Result<SkillReadOutput, SkillStoreError> {
    let (total_len, fingerprint) = inspect_utf8_resource(reader)?;
    let start = parse_pagination_cursor_for_fingerprint(cursor, fingerprint, "skills.read")?;
    if start > total_len || !is_char_boundary_at(reader, start, total_len)? {
        return Err(SkillStoreError::InvalidCursor {
            tool: "skills.read",
        });
    }
    let remaining = total_len - start;
    let mut initial_len = remaining;
    if initial_len > MAX_RESPONSE_BYTES {
        initial_len /= 2;
        while initial_len > MAX_RESPONSE_BYTES {
            initial_len /= 2;
        }
    }
    let mut bytes = read_prefix(reader, start, initial_len)?;
    truncate_to_char_boundary(&mut bytes)?;
    let contents = String::from_utf8(bytes).map_err(|_| SkillStoreError::InvalidUtf8)?;
    let response = |end: usize, next_cursor| SkillReadOutput {
        resource: resource.to_string(),
        contents: contents[..end].to_string(),
        next_cursor,
    };

    let complete = contents.len() == remaining;
    if complete {
        let complete_response = response(contents.len(), None);
        if serialized_len(&complete_response)? <= MAX_RESPONSE_BYTES {
            return Ok(complete_response);
        }
    }

    let mut end = if complete {
        contents.len() / 2
    } else {
        contents.len()
    };
    while end > 0 {
        while !contents.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = response(
            end,
            Some(pagination_cursor_for_fingerprint(fingerprint, start + end)),
        );
        if serialized_len(&candidate)? <= MAX_RESPONSE_BYTES {
            return Ok(candidate);
        }
        end /= 2;
    }
    Err(SkillStoreError::ResponseTooLarge)
}

fn inspect_utf8_resource(reader: &mut (impl Read + Seek)) -> Result<(usize, u64), SkillStoreError> {
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| SkillStoreError::ReadFailed)?;
    let mut hasher = DefaultHasher::new();
    let mut buffer = vec![0_u8; STREAM_READ_BYTES + 3].into_boxed_slice();
    let mut pending = 0;
    let mut total_len = 0_usize;

    loop {
        let read = reader
            .read(&mut buffer[pending..pending + STREAM_READ_BYTES])
            .map_err(|_| SkillStoreError::ReadFailed)?;
        if read == 0 {
            if pending != 0 {
                return Err(SkillStoreError::InvalidUtf8);
            }
            break;
        }
        total_len = total_len
            .checked_add(read)
            .ok_or(SkillStoreError::ReadFailed)?;
        let used = pending + read;
        match std::str::from_utf8(&buffer[..used]) {
            Ok(_) => {
                hasher.write(&buffer[..used]);
                pending = 0;
            }
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                hasher.write(&buffer[..valid]);
                pending = used - valid;
                buffer.copy_within(valid..used, 0);
            }
            Err(_) => return Err(SkillStoreError::InvalidUtf8),
        }
    }
    // `Hash for str` uses DefaultHasher::write_str, whose domain separator is
    // the 0xff byte. Streaming the bytes and adding that separator preserves
    // the pinned content-bound cursor fingerprint without storing the string.
    hasher.write_u8(0xff);
    Ok((total_len, hasher.finish()))
}

fn is_char_boundary_at(
    reader: &mut (impl Read + Seek),
    offset: usize,
    total_len: usize,
) -> Result<bool, SkillStoreError> {
    if offset == total_len {
        return Ok(true);
    }
    reader
        .seek(SeekFrom::Start(
            u64::try_from(offset).map_err(|_| SkillStoreError::ReadFailed)?,
        ))
        .map_err(|_| SkillStoreError::ReadFailed)?;
    let mut byte = [0_u8; 1];
    reader
        .read_exact(&mut byte)
        .map_err(|_| SkillStoreError::ReadFailed)?;
    Ok(byte[0] & 0b1100_0000 != 0b1000_0000)
}

fn read_prefix(
    reader: &mut (impl Read + Seek),
    start: usize,
    len: usize,
) -> Result<Vec<u8>, SkillStoreError> {
    reader
        .seek(SeekFrom::Start(
            u64::try_from(start).map_err(|_| SkillStoreError::ReadFailed)?,
        ))
        .map_err(|_| SkillStoreError::ReadFailed)?;
    let mut bytes = Vec::with_capacity(len);
    let mut buffer = vec![0_u8; STREAM_READ_BYTES].into_boxed_slice();
    while bytes.len() < len {
        let wanted = (len - bytes.len()).min(buffer.len());
        let read = reader
            .read(&mut buffer[..wanted])
            .map_err(|_| SkillStoreError::ReadFailed)?;
        if read == 0 {
            return Err(SkillStoreError::ReadFailed);
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(bytes)
}

fn truncate_to_char_boundary(bytes: &mut Vec<u8>) -> Result<(), SkillStoreError> {
    if let Err(error) = std::str::from_utf8(bytes) {
        if error.error_len().is_some() {
            return Err(SkillStoreError::InvalidUtf8);
        }
        bytes.truncate(error.valid_up_to());
    }
    Ok(())
}

pub(crate) fn serialized_len(value: &impl serde::Serialize) -> Result<usize, SkillStoreError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(SkillStoreError::Serialization)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::pagination_cursor;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    struct TrackingReader {
        inner: Cursor<Vec<u8>>,
        max_read_request: usize,
    }

    impl TrackingReader {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: Cursor::new(bytes),
                max_read_request: 0,
            }
        }
    }

    impl Read for TrackingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.max_read_request = self.max_read_request.max(buffer.len());
            self.inner.read(buffer)
        }
    }

    impl Seek for TrackingReader {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[test]
    fn streaming_pages_are_bounded_and_keep_pinned_content_cursors() {
        let contents = "a💡\n".repeat(350_000);
        let mut cursor = None;
        let mut reconstructed = String::new();

        loop {
            let mut reader = TrackingReader::new(contents.as_bytes().to_vec());
            let page = page_response_from_reader(
                "skill://host/project/large/data.md",
                &mut reader,
                cursor.as_deref(),
            )
            .unwrap();
            assert!(reader.max_read_request <= 64 * 1024);
            assert!(serialized_len(&page).unwrap() <= MAX_RESPONSE_BYTES);
            reconstructed.push_str(&page.contents);
            if let Some(next) = page.next_cursor {
                assert_eq!(
                    next,
                    pagination_cursor(contents.as_str(), reconstructed.len())
                );
                cursor = Some(next);
            } else {
                break;
            }
        }

        assert_eq!(reconstructed, contents);
    }
}
