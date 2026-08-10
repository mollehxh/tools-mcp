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

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchInput {
    pub patch: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedApplyPatch {
    pub patch: String,
    pub operations: Vec<ApplyPatchOperation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyPatchOperation {
    AddFile {
        path: std::path::PathBuf,
        contents: String,
    },
    DeleteFile {
        path: std::path::PathBuf,
    },
    UpdateFile {
        path: std::path::PathBuf,
        move_path: Option<std::path::PathBuf>,
        chunks: Vec<ApplyPatchChunk>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyPatchChunk {
    pub change_context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub context_line_indices: Vec<(usize, usize)>,
    pub is_end_of_file: bool,
}

pub use crate::parser::ParseError as ApplyPatchParseError;

/// Parses the original Codex patch grammar without applying filesystem changes.
///
/// # Errors
///
/// Returns the pinned parser's error when the patch is malformed.
pub fn parse_apply_patch(
    input: &ApplyPatchInput,
) -> Result<ParsedApplyPatch, ApplyPatchParseError> {
    let parsed = crate::parser::parse_patch(&input.patch)?;
    let operations = parsed
        .hunks
        .into_iter()
        .map(ApplyPatchOperation::from)
        .collect();
    Ok(ParsedApplyPatch {
        patch: parsed.patch,
        operations,
    })
}

impl From<crate::parser::Hunk> for ApplyPatchOperation {
    fn from(hunk: crate::parser::Hunk) -> Self {
        match hunk {
            crate::parser::Hunk::AddFile { path, contents } => Self::AddFile { path, contents },
            crate::parser::Hunk::DeleteFile { path } => Self::DeleteFile { path },
            crate::parser::Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => Self::UpdateFile {
                path,
                move_path,
                chunks: chunks.into_iter().map(ApplyPatchChunk::from).collect(),
            },
        }
    }
}

impl From<crate::parser::UpdateFileChunk> for ApplyPatchChunk {
    fn from(chunk: crate::parser::UpdateFileChunk) -> Self {
        Self {
            change_context: chunk.change_context,
            old_lines: chunk.old_lines,
            new_lines: chunk.new_lines,
            context_line_indices: chunk.context_line_indices,
            is_end_of_file: chunk.is_end_of_file,
        }
    }
}
