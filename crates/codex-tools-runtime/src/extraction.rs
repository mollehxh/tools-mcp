/// A minimal compiling slice proving the pinned upstream buffer remains usable.
#[must_use]
pub fn retain_head_and_tail(input: &[u8], max_bytes: usize) -> Vec<u8> {
    let mut buffer = crate::upstream_head_tail_buffer::HeadTailBuffer::new(max_bytes);
    buffer.push_chunk(input.to_vec());
    buffer.to_bytes_with_omission_marker()
}

/// A minimal compiling slice proving the pinned fuzzy patch matcher remains usable.
#[must_use]
pub fn find_patch_context(lines: &[String], pattern: &[String]) -> Option<usize> {
    crate::upstream_seek_sequence::seek_sequence(
        lines,
        pattern,
        0,
        false,
        crate::ApplyPatchFileUpdateMode::NormalizeToLf,
    )
}
