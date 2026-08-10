pub mod contracts;
pub mod extraction;

#[path = "upstream/unified_exec/head_tail_buffer.rs"]
#[allow(dead_code, clippy::needless_pass_by_value)]
mod upstream_head_tail_buffer;
#[path = "upstream/apply_patch/seek_sequence.rs"]
#[allow(dead_code, clippy::items_after_statements)]
mod upstream_seek_sequence;

pub mod unified_exec {
    pub const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1 << 20;

    #[must_use]
    pub fn format_output_omission_marker(omitted_bytes: usize) -> String {
        format!("... {omitted_bytes} bytes omitted ...")
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ApplyPatchFileUpdateMode {
    NormalizeToLf,
    PreserveLineEndings,
}
