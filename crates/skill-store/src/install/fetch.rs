use super::{
    FetchedRepository, GitFetcher, InstallLimits, NormalizedGitSource, RepositoryEntry,
    RepositoryEntryKind, SkillInstallError, controlled_http::ControlledHttp, source::is_commit_id,
    transport::InstallDeadline, tree::accumulate_expansion,
};
use crate::roots::is_portable_segment;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Default)]
pub struct GixGitFetcher;

impl GitFetcher for GixGitFetcher {
    fn fetch(
        &self,
        source: &NormalizedGitSource,
        limits: &InstallLimits,
    ) -> Result<FetchedRepository, SkillInstallError> {
        let deadline = InstallDeadline::new(limits.timeout);
        let temporary = tempfile::tempdir().map_err(|_| SkillInstallError::FetchFailed)?;
        let destination = temporary.path().join("objects");
        let repository = gix::ThreadSafeRepository::init_opts(
            &destination,
            gix::create::Kind::Bare,
            gix::create::Options::default(),
            isolated_open_options(limits),
        )
        .map_err(|_| SkillInstallError::FetchFailed)?
        .to_thread_local();
        let fetch_refspec = fetch_refspec(source)?;
        let remote = repository
            .remote_at_without_url_rewrite(source.repository.as_str())
            .map_err(|_| SkillInstallError::FetchFailed)?
            .with_refspecs([fetch_refspec.as_str()], gix::remote::Direction::Fetch)
            .map_err(|_| SkillInstallError::FetchFailed)?;
        let git_url = gix::Url::try_from(source.repository.as_str())
            .map_err(|_| SkillInstallError::InvalidSource)?;
        let transport = gix_transport::client::blocking_io::http::connect_http(
            ControlledHttp::new(limits.clone(), deadline)?,
            git_url,
            gix_transport::Protocol::V2,
            false,
        );
        let connection = remote.to_connection_with_transport(transport);
        let fetch_options = gix::remote::ref_map::Options::default();
        let pinned_commit = source
            .revision
            .as_deref()
            .filter(|revision| is_commit_id(revision))
            .map(str::to_string);
        let interrupted = Arc::new(AtomicBool::new(false));
        let prepared = connection
            .prepare_fetch(gix::progress::Discard, fetch_options)
            .map_err(|_| SkillInstallError::FetchFailed)?;
        let remaining = deadline.remaining()?;
        let (watchdog_stop, watchdog_rx) = std::sync::mpsc::sync_channel(1);
        let watchdog_flag = Arc::clone(&interrupted);
        let watchdog = std::thread::Builder::new()
            .name("skill-git-deadline".to_string())
            .spawn(move || {
                if watchdog_rx.recv_timeout(remaining).is_err() {
                    watchdog_flag.store(true, Ordering::Release);
                }
            })
            .map_err(|_| SkillInstallError::FetchFailed)?;
        let outcome = prepared.receive(gix::progress::Discard, &interrupted);
        let _ = watchdog_stop.send(());
        let _ = watchdog.join();
        deadline.check()?;
        let outcome = outcome.map_err(|_| SkillInstallError::FetchFailed)?;
        validate_received_pack(&outcome, limits, deadline)?;
        let commit = if let Some(commit) = pinned_commit {
            let commit = gix::ObjectId::from_hex(commit.as_bytes())
                .map_err(|_| SkillInstallError::InvalidRevision)?;
            repository
                .find_object(commit)
                .map_err(|_| SkillInstallError::FetchFailed)?
                .peel_to_commit()
                .map_err(|_| SkillInstallError::FetchFailed)?
        } else if let Some(revision) = source.revision.as_deref() {
            let tracking = tracking_reference(revision);
            repository
                .find_reference(tracking.as_str())
                .map_err(|_| SkillInstallError::FetchFailed)?
                .peel_to_commit()
                .map_err(|_| SkillInstallError::FetchFailed)?
        } else {
            repository
                .find_reference("refs/remotes/origin/HEAD")
                .map_err(|_| SkillInstallError::FetchFailed)?
                .peel_to_commit()
                .map_err(|_| SkillInstallError::FetchFailed)?
        };
        let commit_id = commit.id().to_string();
        deadline.check()?;
        let entries = materialize_commit_tree(&repository, &commit, limits, deadline)?;
        Ok(FetchedRepository {
            repository: source.repository.clone(),
            commit: commit_id,
            entries,
        })
    }
}

fn materialize_commit_tree(
    repository: &gix::Repository,
    commit: &gix::Commit<'_>,
    limits: &InstallLimits,
    deadline: InstallDeadline,
) -> Result<Vec<RepositoryEntry>, SkillInstallError> {
    let tree = commit.tree().map_err(|_| SkillInstallError::FetchFailed)?;
    let mut entries = Vec::new();
    let mut expanded_bytes = 0usize;
    let mut traversal_budget = TraversalBudget::default();
    for entry in tree
        .traverse()
        .breadthfirst
        .files()
        .map_err(|_| SkillInstallError::FetchFailed)?
    {
        deadline.check()?;
        let path = traversal_budget.accept_path(entry.filepath.as_ref(), limits)?;
        let kind = match entry.mode.kind() {
            gix::object::tree::EntryKind::Blob | gix::object::tree::EntryKind::BlobExecutable => {
                RepositoryEntryKind::RegularFile
            }
            gix::object::tree::EntryKind::Link => RepositoryEntryKind::Symlink,
            gix::object::tree::EntryKind::Commit => RepositoryEntryKind::Submodule,
            gix::object::tree::EntryKind::Tree => RepositoryEntryKind::Special,
        };
        if kind != RepositoryEntryKind::RegularFile {
            return Err(SkillInstallError::UnsupportedEntry);
        }
        let object = repository
            .find_object(entry.oid)
            .map_err(|_| SkillInstallError::FetchFailed)?;
        if object.data.len() > limits.max_file_bytes {
            return Err(SkillInstallError::LimitExceeded);
        }
        traversal_budget.accept_file_bytes(object.data.len(), limits)?;
        accumulate_expansion(&mut expanded_bytes, object.data.len(), 0, limits)?;
        let bytes = object.detach().data;
        entries.push(RepositoryEntry { path, kind, bytes });
    }
    Ok(entries)
}

#[derive(Debug, Default)]
struct TraversalBudget {
    files: usize,
    materialized_bytes: usize,
    total_path_bytes: usize,
    portable_paths: HashSet<String>,
}

impl TraversalBudget {
    fn accept_path(
        &mut self,
        raw_path: &[u8],
        limits: &InstallLimits,
    ) -> Result<String, SkillInstallError> {
        if self.files >= limits.max_files
            || raw_path.len() > limits.max_path_bytes
            || self
                .total_path_bytes
                .checked_add(raw_path.len())
                .is_none_or(|total| total > limits.max_total_path_bytes)
        {
            return Err(SkillInstallError::LimitExceeded);
        }
        let path = std::str::from_utf8(raw_path)
            .map_err(|_| SkillInstallError::UnsafePath)?
            .to_string();
        let mut depth = 0usize;
        for component in path.split('/') {
            depth += 1;
            if depth > limits.max_path_depth
                || component.len() > limits.max_segment_bytes
                || !is_portable_segment(component)
            {
                return Err(SkillInstallError::UnsafePath);
            }
        }
        let portable_key = path.nfkc().collect::<String>().to_lowercase();
        if !self.portable_paths.insert(portable_key) {
            return Err(SkillInstallError::PathCollision);
        }
        self.files += 1;
        self.total_path_bytes += raw_path.len();
        Ok(path)
    }

    fn accept_file_bytes(
        &mut self,
        bytes: usize,
        limits: &InstallLimits,
    ) -> Result<(), SkillInstallError> {
        self.materialized_bytes = self
            .materialized_bytes
            .checked_add(bytes)
            .ok_or(SkillInstallError::LimitExceeded)?;
        if self.materialized_bytes > limits.max_materialized_bytes {
            return Err(SkillInstallError::LimitExceeded);
        }
        Ok(())
    }
}

fn isolated_open_options(limits: &InstallLimits) -> gix::open::Options {
    gix::open::Options::isolated().config_overrides(production_config_overrides(limits))
}

fn production_config_overrides(limits: &InstallLimits) -> [String; 7] {
    [
        format!("gitoxide.objects.allocLimit={}", limits.max_object_bytes),
        "credential.helper=".to_string(),
        "core.hooksPath=".to_string(),
        "protocol.allow=never".to_string(),
        "protocol.https.allow=always".to_string(),
        "filter.allow=false".to_string(),
        "submodule.recurse=false".to_string(),
    ]
}

fn fetch_refspec(source: &NormalizedGitSource) -> Result<String, SkillInstallError> {
    match source.revision.as_deref() {
        Some(revision) if is_commit_id(revision) => {
            Ok(format!("+{revision}:refs/remotes/origin/pinned"))
        }
        Some(revision) if is_safe_ref_component(revision) => Ok(format!(
            "+refs/heads/{revision}:{}",
            tracking_reference(revision)
        )),
        Some(_) => Err(SkillInstallError::InvalidRevision),
        None => Ok("+HEAD:refs/remotes/origin/HEAD".to_string()),
    }
}

fn tracking_reference(revision: &str) -> String {
    format!("refs/remotes/origin/{revision}")
}

fn is_safe_ref_component(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && !value.ends_with('.')
        && !value.ends_with('/')
        && !std::path::Path::new(value)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("lock"))
        && !value.contains("..")
        && !value.contains("@{")
        && !value.bytes().any(|byte| {
            byte <= b' ' || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

fn validate_received_pack(
    outcome: &gix::remote::fetch::Outcome,
    limits: &InstallLimits,
    deadline: InstallDeadline,
) -> Result<(), SkillInstallError> {
    let gix::remote::fetch::Status::Change {
        write_pack_bundle, ..
    } = &outcome.status
    else {
        return Ok(());
    };
    let Some(bundle) = write_pack_bundle.to_bundle() else {
        return Ok(());
    };
    let bundle = bundle.map_err(|_| SkillInstallError::FetchFailed)?;
    let mut inflate = gix_zlib::Inflate::default();
    let object_count = usize::try_from(bundle.index.num_objects())
        .map_err(|_| SkillInstallError::LimitExceeded)?;
    if object_count > limits.max_objects {
        return Err(SkillInstallError::LimitExceeded);
    }
    let mut expanded_bytes = 0usize;
    for indexed in bundle.index.iter() {
        deadline.check()?;
        let entry = bundle
            .pack
            .entry(indexed.pack_offset)
            .map_err(|_| SkillInstallError::FetchFailed)?;
        let header = bundle
            .pack
            .decode_header(entry.clone(), &mut inflate, &|id| {
                let index = bundle.index.lookup(id)?;
                let pack_offset = bundle.index.pack_offset_at_index(index);
                bundle
                    .pack
                    .entry(pack_offset)
                    .ok()
                    .map(gix_pack::data::decode::header::ResolvedBase::InPack)
            })
            .map_err(|_| SkillInstallError::FetchFailed)?;
        let object_size =
            usize::try_from(header.object_size).map_err(|_| SkillInstallError::LimitExceeded)?;
        let delta_size = if entry.header.is_delta() {
            usize::try_from(entry.decompressed_size)
                .map_err(|_| SkillInstallError::LimitExceeded)?
        } else {
            0
        };
        accumulate_expansion(&mut expanded_bytes, object_size, delta_size, limits)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize_git_source;

    #[test]
    fn production_fetch_isolates_host_git_configuration_and_execution_surfaces() {
        let limits = InstallLimits::default();
        let options = isolated_open_options(&limits);
        assert_eq!(options.permissions, gix::open::Permissions::isolated());
        assert_eq!(
            production_config_overrides(&limits),
            [
                format!("gitoxide.objects.allocLimit={}", limits.max_object_bytes),
                "credential.helper=".to_string(),
                "core.hooksPath=".to_string(),
                "protocol.allow=never".to_string(),
                "protocol.https.allow=always".to_string(),
                "filter.allow=false".to_string(),
                "submodule.recurse=false".to_string(),
            ]
        );

        let temporary = tempfile::tempdir().unwrap();
        let repository = gix::ThreadSafeRepository::init_opts(
            temporary.path().join("isolated.git"),
            gix::create::Kind::Bare,
            gix::create::Options::default(),
            isolated_open_options(&limits),
        )
        .unwrap()
        .to_thread_local();
        let config = repository.config_snapshot();
        assert_eq!(config.string("credential.helper").unwrap(), "");
        assert_eq!(config.string("core.hooksPath").unwrap(), "");
        assert_eq!(config.string("protocol.allow").unwrap(), "never");
        assert_eq!(config.string("protocol.https.allow").unwrap(), "always");
        assert_eq!(config.boolean("filter.allow"), Some(false));
        assert_eq!(config.boolean("submodule.recurse"), Some(false));
    }

    #[test]
    fn fetch_refspec_is_narrow_and_never_fetches_all_heads() {
        let mut source = normalize_git_source("https://example.com/repo.git", None, None).unwrap();
        assert_eq!(
            fetch_refspec(&source).unwrap(),
            "+HEAD:refs/remotes/origin/HEAD"
        );
        source.revision = Some("main".to_string());
        assert_eq!(
            fetch_refspec(&source).unwrap(),
            "+refs/heads/main:refs/remotes/origin/main"
        );
        assert!(!fetch_refspec(&source).unwrap().contains('*'));
    }

    #[test]
    fn traversal_budget_rejects_deep_and_aggregate_paths_before_accumulation() {
        let limits = InstallLimits {
            max_files: 3,
            max_path_bytes: 32,
            max_path_depth: 2,
            max_segment_bytes: 8,
            max_total_path_bytes: 12,
            max_materialized_bytes: 4,
            ..InstallLimits::default()
        };
        let mut budget = TraversalBudget::default();

        assert_eq!(budget.accept_path(b"a/one", &limits).unwrap(), "a/one");
        assert_eq!(budget.accept_path(b"b/two", &limits).unwrap(), "b/two");
        assert!(budget.accept_path(b"c/three", &limits).is_err());

        let mut deep = TraversalBudget::default();
        assert!(deep.accept_path(b"a/b/c", &limits).is_err());
        assert!(deep.accept_path(b"segmentxx/file", &limits).is_err());

        assert!(budget.accept_file_bytes(4, &limits).is_ok());
        assert!(budget.accept_file_bytes(1, &limits).is_err());
    }
}
