use super::ApplyPatchError;
use crate::ApplyPatchFileUpdateMode;
use crate::contracts::{ApplyPatchInput, ApplyPatchOutput};
use crate::parser::{Hunk, UpdateFileChunk};
use mcp_agent_authority::{WorkspaceAuthority, WorkspaceOperations};
use std::path::{Path, PathBuf};

struct PreparedHunk {
    hunk: Hunk,
    source: PathBuf,
    destination: Option<PathBuf>,
}

/// Applies the original Codex patch grammar through the fixed workspace
/// authority. Path-policy validation for every hunk completes before the first
/// mutation; content/application failures retain upstream partial-patch
/// behavior after that security preflight.
///
/// # Errors
///
/// Returns the pinned parser or application diagnostic, or a fixed-workspace
/// policy/filesystem error. A policy error occurs before any mutation.
pub fn apply_patch(
    authority: &WorkspaceAuthority,
    input: &ApplyPatchInput,
) -> Result<ApplyPatchOutput, ApplyPatchError> {
    let parsed = crate::parser::parse_patch(&input.patch).map_err(ApplyPatchError::Parse)?;
    if parsed.hunks.is_empty() {
        return Err(ApplyPatchError::NoFilesModified);
    }

    let prepared = parsed
        .hunks
        .into_iter()
        .map(|hunk| {
            let (source_path, destination_path) = match &hunk {
                Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => (path, None),
                Hunk::UpdateFile {
                    path, move_path, ..
                } => (path, move_path.as_ref()),
            };
            let source = authorize_relative(authority, source_path)?;
            let destination = destination_path
                .map(|path| authorize_relative(authority, path))
                .transpose()?;
            Ok(PreparedHunk {
                hunk,
                source,
                destination,
            })
        })
        .collect::<Result<Vec<_>, ApplyPatchError>>()?;

    let operations = WorkspaceOperations::new(authority).map_err(|source| {
        ApplyPatchError::filesystem("Failed to open fixed workspace".to_string(), source)
    })?;
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    for prepared_hunk in prepared {
        let (kind, affected_path) = apply_prepared_hunk(&operations, prepared_hunk)?;
        match kind {
            AppliedKind::Added => added.push(affected_path),
            AppliedKind::Modified => modified.push(affected_path),
            AppliedKind::Deleted => deleted.push(affected_path),
        }
    }

    let mut output = String::from("Success. Updated the following files:\n");
    append_summary(&mut output, 'A', &added);
    append_summary(&mut output, 'M', &modified);
    append_summary(&mut output, 'D', &deleted);
    Ok(ApplyPatchOutput { output })
}

enum AppliedKind {
    Added,
    Modified,
    Deleted,
}

fn apply_prepared_hunk(
    operations: &WorkspaceOperations,
    prepared: PreparedHunk,
) -> Result<(AppliedKind, PathBuf), ApplyPatchError> {
    let affected = affected_path(&prepared.hunk).to_path_buf();
    let source_display = source_path(&prepared.hunk).to_path_buf();
    let summary_path = source_display.clone();
    match prepared.hunk {
        Hunk::AddFile { contents, .. } => {
            operations
                .atomic_write(&prepared.source, contents.as_bytes())
                .map_err(|source| {
                    ApplyPatchError::filesystem(
                        format!("Failed to write file {}", affected.display()),
                        source,
                    )
                })?;
            Ok((AppliedKind::Added, summary_path))
        }
        Hunk::DeleteFile { .. } => {
            operations.remove_file(&prepared.source).map_err(|source| {
                ApplyPatchError::filesystem(
                    format!("Failed to delete file {}", affected.display()),
                    source,
                )
            })?;
            Ok((AppliedKind::Deleted, summary_path))
        }
        Hunk::UpdateFile { chunks, .. } => {
            let original = operations
                .read_to_string(&prepared.source)
                .map_err(|source| {
                    ApplyPatchError::filesystem(
                        format!("Failed to read file to update {}", affected.display()),
                        source,
                    )
                })?;
            let updated = derive_new_contents_from_chunks(
                &original,
                &affected,
                &chunks,
                ApplyPatchFileUpdateMode::NormalizeToLf,
            )?;
            let write_path = prepared.destination.as_deref().unwrap_or(&prepared.source);
            operations
                .atomic_write(write_path, updated.as_bytes())
                .map_err(|source| {
                    ApplyPatchError::filesystem(
                        format!("Failed to write file {}", affected.display()),
                        source,
                    )
                })?;
            if prepared.destination.is_some() {
                operations.remove_file(&prepared.source).map_err(|source| {
                    ApplyPatchError::filesystem(
                        format!("Failed to remove original {}", source_display.display()),
                        source,
                    )
                })?;
            }
            Ok((AppliedKind::Modified, summary_path))
        }
    }
}

fn authorize_relative(
    authority: &WorkspaceAuthority,
    path: &Path,
) -> Result<PathBuf, ApplyPatchError> {
    let absolute = authority
        .command()
        .authorize_write(path)
        .map_err(ApplyPatchError::Policy)?;
    absolute
        .strip_prefix(authority.workspace_root())
        .map(Path::to_path_buf)
        .map_err(|_| ApplyPatchError::Policy(mcp_agent_authority::AuthorityError::OutsideWorkspace))
}

fn source_path(hunk: &Hunk) -> &Path {
    match hunk {
        Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } | Hunk::UpdateFile { path, .. } => {
            path
        }
    }
}

fn affected_path(hunk: &Hunk) -> &Path {
    match hunk {
        Hunk::UpdateFile {
            move_path: Some(path),
            ..
        } => path,
        _ => source_path(hunk),
    }
}

fn append_summary(output: &mut String, status: char, paths: &[PathBuf]) {
    use std::fmt::Write as _;
    for path in paths {
        let _ = writeln!(output, "{status} {}", path.display());
    }
}

fn derive_new_contents_from_chunks(
    original_contents: &str,
    path: &Path,
    chunks: &[UpdateFileChunk],
    update_mode: ApplyPatchFileUpdateMode,
) -> Result<String, ApplyPatchError> {
    let mut original_lines = original_contents
        .split('\n')
        .map(String::from)
        .collect::<Vec<_>>();
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let replacements = compute_replacements(&original_lines, path, chunks, update_mode)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);
    if !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join("\n"))
}

type Replacement = (usize, usize, Vec<String>);

fn compute_replacements(
    original_lines: &[String],
    path: &Path,
    chunks: &[UpdateFileChunk],
    update_mode: ApplyPatchFileUpdateMode,
) -> Result<Vec<Replacement>, ApplyPatchError> {
    let mut replacements = Vec::new();
    let mut line_index = 0;

    for chunk in chunks {
        if let Some(context) = &chunk.change_context {
            if let Some(index) = crate::upstream_seek_sequence::seek_sequence(
                original_lines,
                std::slice::from_ref(context),
                line_index,
                false,
                update_mode,
            ) {
                line_index = index + 1;
            } else {
                return Err(ApplyPatchError::ComputeReplacements(format!(
                    "Failed to find context '{context}' in {}",
                    path.display()
                )));
            }
        }

        if chunk.old_lines.is_empty() {
            replacements.push((original_lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern = chunk.old_lines.as_slice();
        let mut new_slice = chunk.new_lines.as_slice();
        let mut found = crate::upstream_seek_sequence::seek_sequence(
            original_lines,
            pattern,
            line_index,
            chunk.is_end_of_file,
            update_mode,
        );
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = crate::upstream_seek_sequence::seek_sequence(
                original_lines,
                pattern,
                line_index,
                chunk.is_end_of_file,
                update_mode,
            );
        }

        let Some(start_index) = found else {
            return Err(ApplyPatchError::ComputeReplacements(format!(
                "Failed to find expected lines in {}:\n{}",
                path.display(),
                chunk.old_lines.join("\n")
            )));
        };
        replacements.push((start_index, pattern.len(), new_slice.to_vec()));
        line_index = start_index + pattern.len();
    }

    replacements.sort_by_key(|(index, _, _)| *index);
    Ok(replacements)
}

fn apply_replacements(mut lines: Vec<String>, replacements: &[Replacement]) -> Vec<String> {
    for (start_index, old_len, new_segment) in replacements.iter().rev() {
        for _ in 0..*old_len {
            if *start_index < lines.len() {
                lines.remove(*start_index);
            }
        }
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(*start_index + offset, new_line.clone());
        }
    }
    lines
}
