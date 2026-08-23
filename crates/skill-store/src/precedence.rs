use crate::contracts::ListedSkill;

pub(crate) fn resolve_name<'a>(
    system: &'a [ListedSkill],
    project: &'a [ListedSkill],
    global: &'a [ListedSkill],
    name: &str,
) -> Option<&'a ListedSkill> {
    if name == "skill-installer"
        && let Some(installer) = system.iter().find(|skill| skill.name == name)
    {
        return Some(installer);
    }
    project
        .iter()
        .find(|skill| skill.name == name)
        .or_else(|| global.iter().find(|skill| skill.name == name))
        .or_else(|| system.iter().find(|skill| skill.name == name))
}
