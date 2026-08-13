use mcp_agent_authority::WorkspaceAuthority;
use skill_store::{SkillCatalog, SkillListInput, SkillReadInput, SkillScope};
use std::fs;

fn fixture() -> (tempfile::TempDir, WorkspaceAuthority) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let global = root.path().join("global");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&global).unwrap();
    let authority =
        WorkspaceAuthority::with_global_skills(&workspace, global.canonicalize().unwrap()).unwrap();
    (root, authority)
}

fn write_skill(authority: &WorkspaceAuthority, package: &str, description: &str) {
    let root = authority.project_skills().root().join(package);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("SKILL.md"),
        format!("---\nname: {package}\ndescription: {description}\n---\nbody"),
    )
    .unwrap();
}

#[test]
fn list_uses_pinned_twenty_item_pages_and_cursor_shape() {
    let (_root, authority) = fixture();
    for index in 0..21 {
        write_skill(&authority, &format!("skill-{index:02}"), "description");
    }
    let catalog = SkillCatalog::new(&authority).unwrap();
    let first = catalog
        .list(&SkillListInput {
            scope: SkillScope::Project,
            cursor: None,
        })
        .unwrap();
    assert_eq!(first.skills.len(), 20);
    let cursor = first.next_cursor.unwrap();
    let (fingerprint, offset) = cursor.split_once(':').unwrap();
    assert_eq!(fingerprint.len(), 16);
    assert!(
        fingerprint
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    );
    assert_eq!(offset, "20");

    let second = catalog
        .list(&SkillListInput {
            scope: SkillScope::Project,
            cursor: Some(cursor),
        })
        .unwrap();
    assert_eq!(second.skills.len(), 1);
    assert!(second.warnings.is_empty());
    assert_eq!(second.next_cursor, None);
}

#[test]
fn malformed_and_stale_list_cursors_have_pinned_errors() {
    let (_root, authority) = fixture();
    for index in 0..21 {
        write_skill(&authority, &format!("skill-{index:02}"), "before");
    }
    let catalog = SkillCatalog::new(&authority).unwrap();
    let cursor = catalog
        .list(&SkillListInput {
            scope: SkillScope::Project,
            cursor: None,
        })
        .unwrap()
        .next_cursor
        .unwrap();
    assert_eq!(
        catalog
            .list(&SkillListInput {
                scope: SkillScope::Project,
                cursor: Some("not-a-cursor".to_string())
            })
            .unwrap_err()
            .to_string(),
        "skills.list cursor is invalid"
    );

    write_skill(&authority, "skill-00", "after");
    assert_eq!(
        catalog
            .list(&SkillListInput {
                scope: SkillScope::Project,
                cursor: Some(cursor)
            })
            .unwrap_err()
            .to_string(),
        "skills.list cursor is stale; restart from the first page"
    );
}

#[test]
fn read_cursor_is_content_bound_and_restart_compatible() {
    let (_root, authority) = fixture();
    write_skill(&authority, "large", "large resource");
    let resource_path = authority.project_skills().root().join("large/data.md");
    fs::write(&resource_path, "a".repeat(700 * 1024)).unwrap();
    let input = |cursor| SkillReadInput {
        scope: SkillScope::Project,
        package: "large".to_string(),
        resource: "skill://host/project/large/data.md".to_string(),
        cursor,
    };
    let catalog = SkillCatalog::new(&authority).unwrap();
    let cursor = catalog.read(&input(None)).unwrap().next_cursor.unwrap();

    let rebuilt = SkillCatalog::new(&authority).unwrap();
    rebuilt
        .read(&input(Some(cursor.clone())))
        .expect("unchanged resource cursor survives restart");

    fs::write(&resource_path, "b".repeat(700 * 1024)).unwrap();
    assert_eq!(
        catalog.read(&input(Some(cursor))).unwrap_err().to_string(),
        "skills.read cursor is stale; restart from the first page"
    );
    assert_eq!(
        catalog
            .read(&input(Some("xyz:1".to_string())))
            .unwrap_err()
            .to_string(),
        "skills.read cursor is stale; restart from the first page"
    );
    assert_eq!(
        catalog
            .read(&input(Some("gggggggggggggggg:1".to_string())))
            .unwrap_err()
            .to_string(),
        "skills.read cursor is stale; restart from the first page"
    );
    assert_eq!(
        catalog
            .read(&input(Some(":1".to_string())))
            .unwrap_err()
            .to_string(),
        "skills.read cursor is stale; restart from the first page"
    );
    assert_eq!(
        catalog
            .read(&input(Some("not-a-cursor".to_string())))
            .unwrap_err()
            .to_string(),
        "skills.read cursor is invalid"
    );
    let fingerprint = catalog
        .read(&input(None))
        .unwrap()
        .next_cursor
        .unwrap()
        .split_once(':')
        .unwrap()
        .0
        .to_string();
    assert_eq!(
        catalog
            .read(&input(Some(format!("{fingerprint}:not-an-offset"))))
            .unwrap_err()
            .to_string(),
        "skills.read cursor is invalid"
    );
}
