#![allow(dead_code)]

use mcp_agent_authority::WorkspaceAuthority;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Fixture {
    _root: tempfile::TempDir,
    pub workspace: PathBuf,
    pub outside: PathBuf,
    global_skills: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        let global_skills = outside.join("global/.agents/skills");
        for path in [
            workspace.join("src"),
            workspace.join(".git"),
            workspace.join(".codex"),
            workspace.join(".mcp-agent/staging"),
            workspace.join(".agents/skills"),
            global_skills.clone(),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        Self {
            _root: root,
            workspace,
            outside,
            global_skills: global_skills.canonicalize().unwrap(),
        }
    }

    pub fn authority(&self) -> WorkspaceAuthority {
        WorkspaceAuthority::with_global_skills(&self.workspace, &self.global_skills).unwrap()
    }

    pub fn release_dir(&self) -> PathBuf {
        let release = self.outside.join("release");
        fs::create_dir_all(&release).unwrap();
        release
    }
}

#[cfg(unix)]
pub fn make_dir_symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
pub fn make_dir_symlink(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap();
}
