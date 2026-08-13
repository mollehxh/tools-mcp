use super::SkillInstallError;
use std::net::{IpAddr, Ipv4Addr};
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedGitSource {
    pub repository: String,
    pub selector: Option<String>,
    pub revision: Option<String>,
}

/// Normalizes a supported public HTTPS Git source.
///
/// # Errors
///
/// Returns an error for unsafe URLs, conflicting GitHub-tree components, or
/// invalid selectors and revisions.
pub fn normalize_git_source(
    repository: &str,
    selector: Option<&str>,
    revision: Option<&str>,
) -> Result<NormalizedGitSource, SkillInstallError> {
    let url = Url::parse(repository).map_err(|_| SkillInstallError::InvalidSource)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SkillInstallError::InvalidSource);
    }
    let host = url.host_str().ok_or(SkillInstallError::InvalidSource)?;
    if url.port().is_some_and(|port| port != 443) {
        return Err(SkillInstallError::InvalidSource);
    }
    if host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok_and(is_non_public_ip)
    {
        return Err(SkillInstallError::NonPublicSource);
    }

    let path = url.path().trim_end_matches('/');
    let github_tree = host.eq_ignore_ascii_case("github.com")
        && path.split('/').filter(|part| !part.is_empty()).count() >= 4
        && path.contains("/tree/");
    if host.eq_ignore_ascii_case("github.com") && path.contains("/tree/") && !github_tree {
        return Err(SkillInstallError::InvalidSource);
    }
    let (repository, tree_selector, tree_revision) = if github_tree {
        parse_github_tree(&url)?
    } else {
        (
            url.to_string().trim_end_matches('/').to_string(),
            None,
            None,
        )
    };
    let selector = match (selector, tree_selector.as_deref()) {
        (Some(_), Some(_)) => return Err(SkillInstallError::AmbiguousSource),
        (Some(value), None) | (None, Some(value)) => Some(normalize_selector(value)?),
        (None, None) => None,
    };
    let revision = match (revision, tree_revision) {
        (Some(_), Some(_)) => return Err(SkillInstallError::AmbiguousSource),
        (Some(value), None) => Some(normalize_revision(value)?),
        (None, Some(value)) => Some(normalize_revision(&value)?),
        (None, None) => None,
    };
    Ok(NormalizedGitSource {
        repository,
        selector,
        revision,
    })
}

fn parse_github_tree(
    url: &Url,
) -> Result<(String, Option<String>, Option<String>), SkillInstallError> {
    let parts = url
        .path_segments()
        .ok_or(SkillInstallError::InvalidSource)?
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let tree = parts
        .iter()
        .position(|part| *part == "tree")
        .ok_or(SkillInstallError::InvalidSource)?;
    if tree != 2 || parts.len() < 4 {
        return Err(SkillInstallError::InvalidSource);
    }
    let revision = parts[3];
    if parts.len() > 4 && !matches!(revision, "main" | "master") && !is_commit_id(revision) {
        // GitHub's `/tree/<revision>/<path>` route has no delimiter between a
        // slash-containing branch and the subtree. Without advertised refs,
        // treating the first segment as the branch can select the wrong tree.
        // Keep common default branches and immutable commits ergonomic; all
        // other subtree installs must use the repository URL with explicit
        // revision and selector fields.
        return Err(SkillInstallError::AmbiguousSource);
    }
    let repository = format!("https://github.com/{}/{}.git", parts[0], parts[1]);
    let revision = Some(revision.to_string());
    let selector = (parts.len() > 4).then(|| parts[4..].join("/"));
    Ok((repository, selector, revision))
}

fn normalize_selector(value: &str) -> Result<String, SkillInstallError> {
    let normalized = value.trim_matches('/');
    if normalized.is_empty()
        || normalized
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains('\\'))
    {
        return Err(SkillInstallError::InvalidSelector);
    }
    Ok(normalized.to_string())
}

fn normalize_revision(value: &str) -> Result<String, SkillInstallError> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('-')
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(SkillInstallError::InvalidRevision);
    }
    Ok(value.to_string())
}

pub(crate) fn is_non_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !is_public_ipv4(ip),
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            let value = u128::from_be_bytes(ip.octets());
            (segments[0] & 0xe000) != 0x2000
                || ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || ip.is_unique_local()
                || (segments[0] & 0xff00) == 0xfe00
                || (segments[0] & 0xffc0) == 0xfe80
                || matches!(segments, [0, 0, 0, 0, 0, 0xffff, _, _])
                || matches!(segments, [0x64, 0xff9b, 1, _, _, _, _, _])
                || matches!(segments, [0x100, 0, 0, 0, _, _, _, _])
                || (matches!(segments, [0x2001, second, _, _, _, _, _, _] if second < 0x200)
                    && !(value == 0x2001_0001_0000_0000_0000_0000_0000_0001
                        || value == 0x2001_0001_0000_0000_0000_0000_0000_0002
                        || matches!(segments, [0x2001, 3, _, _, _, _, _, _])
                        || matches!(segments, [0x2001, 4, 0x112, _, _, _, _, _])
                        || matches!(segments, [0x2001, second, _, _, _, _, _, _] if (0x20..=0x3f).contains(&second))))
                || matches!(segments, [0x2002, _, _, _, _, _, _, _])
                || matches!(segments, [0x2001, 0xdb8, ..] | [0x3fff, 0..=0x0fff, ..])
                || matches!(segments, [0x5f00, ..])
        }
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [first, second, third, fourth] = ip.octets();
    !(first == 0
        || ip.is_private()
        || (first == 100 && (second & 0xc0) == 64)
        || ip.is_loopback()
        || ip.is_link_local()
        || (first == 192 && second == 0 && third == 0 && !matches!(fourth, 9 | 10))
        || (first == 192 && second == 88 && third == 99)
        || ip.is_documentation()
        || (first == 198 && matches!(second, 18 | 19))
        || first >= 224
        || ip.is_broadcast())
}

pub(crate) fn is_commit_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
