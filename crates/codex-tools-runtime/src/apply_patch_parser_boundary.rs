use std::path::PathBuf;
use thiserror::Error;

pub(crate) const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
pub(crate) const END_PATCH_MARKER: &str = "*** End Patch";
pub(crate) const ADD_FILE_MARKER: &str = "*** Add File: ";
pub(crate) const DELETE_FILE_MARKER: &str = "*** Delete File: ";
pub(crate) const UPDATE_FILE_MARKER: &str = "*** Update File: ";
pub(crate) const MOVE_TO_MARKER: &str = "*** Move to: ";
pub(crate) const EOF_MARKER: &str = "*** End of File";
pub(crate) const CHANGE_CONTEXT_MARKER: &str = "@@ ";
pub(crate) const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ParseError {
    #[error("invalid patch: {0}")]
    InvalidPatchError(String),
    #[error("invalid hunk at line {line_number}, {message}")]
    InvalidHunkError { message: String, line_number: usize },
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Hunk {
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<UpdateFileChunk>,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpdateFileChunk {
    pub change_context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub context_line_indices: Vec<(usize, usize)>,
    pub is_end_of_file: bool,
}

impl UpdateFileChunk {
    pub(crate) fn push_context_line(&mut self, line: String) {
        self.context_line_indices
            .push((self.old_lines.len(), self.new_lines.len()));
        self.old_lines.push(line.clone());
        self.new_lines.push(line);
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct ParsedPatch {
    pub(crate) patch: String,
    pub(crate) hunks: Vec<Hunk>,
}

pub(crate) fn parse_patch(patch: &str) -> Result<ParsedPatch, ParseError> {
    let lines = patch.trim().lines().collect::<Vec<_>>();
    let patch_lines = match check_patch_boundaries(&lines) {
        Ok(lines) => lines,
        Err(original_error) => match lines.as_slice() {
            [first, .., last]
                if matches!(*first, "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
                    && last.ends_with("EOF")
                    && lines.len() >= 4 =>
            {
                check_patch_boundaries(&lines[1..lines.len() - 1])?
            }
            _ => return Err(original_error),
        },
    };

    let patch = patch_lines.join("\n");
    let mut parser = crate::upstream_streaming_patch_parser::StreamingPatchParser::default();
    parser.push_delta(&patch)?;
    let hunks = parser.finish()?;
    Ok(ParsedPatch { patch, hunks })
}

fn check_patch_boundaries<'a>(lines: &'a [&'a str]) -> Result<&'a [&'a str], ParseError> {
    let (first, last) = match lines {
        [] => (None, None),
        [only] => (Some(*only), Some(*only)),
        [first, .., last] => (Some(*first), Some(*last)),
    };
    match (first.map(str::trim), last.map(str::trim)) {
        (Some(BEGIN_PATCH_MARKER), Some(END_PATCH_MARKER)) => Ok(lines),
        (Some(first), _) if first != BEGIN_PATCH_MARKER => Err(ParseError::InvalidPatchError(
            "The first line of the patch must be '*** Begin Patch'".to_string(),
        )),
        _ => Err(ParseError::InvalidPatchError(
            "The last line of the patch must be '*** End Patch'".to_string(),
        )),
    }
}
