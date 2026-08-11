use cap_std::fs::Dir;
use std::path::{Path, PathBuf};

/// A server-owned filesystem root. Possessing this value is the authority to
/// perform managed writes beneath the root; command children never receive it.
#[derive(Debug)]
pub struct ManagedRoot {
    root: PathBuf,
    scope: ManagedWriteScope,
    dir: Dir,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedWriteScope {
    ProjectSkills,
    GlobalSkills,
    ServerStaging,
}

impl ManagedRoot {
    pub(crate) fn new(root: PathBuf, scope: ManagedWriteScope, dir: Dir) -> Self {
        Self { root, scope, dir }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn scope(&self) -> ManagedWriteScope {
        self.scope
    }

    pub(crate) fn try_clone_dir(&self) -> std::io::Result<Dir> {
        self.dir.try_clone()
    }
}
