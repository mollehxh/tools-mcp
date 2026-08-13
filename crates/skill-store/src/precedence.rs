use crate::contracts::ListedSkill;

pub(crate) fn resolve_name<'a>(
    project: &'a [ListedSkill],
    global: &'a [ListedSkill],
    name: &str,
) -> Option<&'a ListedSkill> {
    project
        .iter()
        .find(|skill| skill.name == name)
        .or_else(|| global.iter().find(|skill| skill.name == name))
}
