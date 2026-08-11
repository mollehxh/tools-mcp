pub mod contracts;
pub mod extraction;
pub mod process;

#[path = "apply_patch_parser_boundary.rs"]
mod parser;
#[path = "upstream/unified_exec/head_tail_buffer.rs"]
#[allow(dead_code, clippy::needless_pass_by_value)]
mod upstream_head_tail_buffer;
#[path = "upstream/apply_patch/seek_sequence.rs"]
#[allow(dead_code, clippy::items_after_statements)]
mod upstream_seek_sequence;
#[path = "upstream/apply_patch/streaming_parser.rs"]
#[allow(
    dead_code,
    clippy::enum_glob_use,
    clippy::match_same_arms,
    clippy::too_many_lines
)]
mod upstream_streaming_patch_parser;

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
