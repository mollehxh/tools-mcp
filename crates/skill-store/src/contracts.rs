use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillScope {
    Project,
    Global,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillAuthority {
    Host,
}

impl SkillScope {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillListInput {
    pub scope: SkillScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSourceKind {
    Host,
    Git,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSource {
    pub kind: SkillSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

impl SkillSource {
    pub(crate) const fn host() -> Self {
        Self {
            kind: SkillSourceKind::Host,
            repository: None,
            commit: None,
            selector: None,
        }
    }

    pub(crate) fn git(repository: String, commit: String, selector: Option<String>) -> Self {
        Self {
            kind: SkillSourceKind::Git,
            repository: Some(repository),
            commit: Some(commit),
            selector,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillInstallInput {
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub scope: SkillScope,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillInstallOutput {
    pub authority: SkillAuthority,
    pub scope: SkillScope,
    pub package: String,
    pub name: String,
    pub main_resource: String,
    pub source: SkillSource,
}

impl SkillInstallOutput {
    pub(crate) fn new(
        scope: SkillScope,
        package: String,
        name: String,
        source: SkillSource,
    ) -> Self {
        let main_resource = format!("skill://host/{}/{package}/SKILL.md", scope.as_str());
        Self {
            authority: SkillAuthority::Host,
            scope,
            package,
            name,
            main_resource,
            source,
        }
    }

    #[must_use]
    pub fn read_input(&self) -> SkillReadInput {
        SkillReadInput {
            scope: self.scope,
            package: self.package.clone(),
            resource: self.main_resource.clone(),
            cursor: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSkillMetadata {
    pub package: String,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListedSkill {
    pub authority: SkillAuthority,
    pub scope: SkillScope,
    pub package: String,
    pub name: String,
    pub description: String,
    pub main_resource: String,
    pub source: SkillSource,
}

impl ListedSkill {
    pub(crate) fn from_host(scope: SkillScope, metadata: HostSkillMetadata) -> Self {
        let main_resource = format!(
            "skill://host/{}/{}/SKILL.md",
            scope.as_str(),
            metadata.package
        );
        Self {
            authority: SkillAuthority::Host,
            scope,
            package: metadata.package,
            name: metadata.name,
            description: metadata.description,
            main_resource,
            source: SkillSource::host(),
        }
    }

    #[must_use]
    pub fn read_input(&self, cursor: Option<String>) -> SkillReadInput {
        SkillReadInput {
            scope: self.scope,
            package: self.package.clone(),
            resource: self.main_resource.clone(),
            cursor,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillListOutput {
    pub skills: Vec<ListedSkill>,
    pub warnings: Vec<String>,
    pub next_cursor: Option<String>,
}

impl SkillListOutput {
    #[must_use]
    pub fn from_host_page(
        scope: SkillScope,
        skills: Vec<HostSkillMetadata>,
        warnings: Vec<String>,
        next_cursor: Option<String>,
    ) -> Self {
        Self {
            skills: skills
                .into_iter()
                .map(|skill| ListedSkill::from_host(scope, skill))
                .collect(),
            warnings,
            next_cursor,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillReadInput {
    pub scope: SkillScope,
    pub package: String,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillReadOutput {
    pub resource: String,
    pub contents: String,
    pub next_cursor: Option<String>,
}
