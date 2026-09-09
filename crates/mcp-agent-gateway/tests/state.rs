use mcp_agent_gateway::{AllocationKind, StateStore};

#[test]
fn allocator_survives_reopen_and_stale_database_rollback() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let watermark = root.path().join("allocator.watermark");
    {
        let store = StateStore::open(&database, &watermark).unwrap();
        assert_eq!(store.allocate(AllocationKind::Generation).unwrap(), 2);
        assert_eq!(store.allocate(AllocationKind::Terminal).unwrap(), 1_000);
    }
    {
        let store = StateStore::open(&database, &watermark).unwrap();
        assert_eq!(store.allocate(AllocationKind::Generation).unwrap(), 3);
        assert_eq!(store.allocate(AllocationKind::Terminal).unwrap(), 1_001);
    }
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE allocations SET value = 1 WHERE kind = 'generation'",
            [],
        )
        .unwrap();
    let store = StateStore::open(&database, &watermark).unwrap();
    assert_eq!(store.allocate(AllocationKind::Generation).unwrap(), 4);
}

#[test]
fn malformed_rollback_watermark_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let watermark = root.path().join("allocator.watermark");
    std::fs::write(&watermark, "generation=not-a-number\n").unwrap();
    assert!(StateStore::open(&database, &watermark).is_err());
}

#[test]
fn missing_rollback_watermark_blocks_reopening_existing_state() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let watermark = root.path().join("allocator.watermark");
    let store = StateStore::open(&database, &watermark).unwrap();
    store.allocate(AllocationKind::Generation).unwrap();
    drop(store);
    std::fs::remove_file(&watermark).unwrap();

    assert!(StateStore::open(&database, &watermark).is_err());
}

#[test]
fn superseded_launch_tombstones_survive_restart_and_fail_closed_at_capacity() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let watermark = root.path().join("allocator.watermark");
    {
        let store = StateStore::open(&database, &watermark).unwrap();
        store.allocate(AllocationKind::Generation).unwrap();
        store.record_superseded_launch("launch-a").unwrap();
        assert!(store.is_launch_superseded("launch-a").unwrap());
        assert!(!store.is_launch_superseded("launch-b").unwrap());
    }

    let store = StateStore::open(&database, &watermark).unwrap();
    assert!(store.is_launch_superseded("launch-a").unwrap());
    for index in 1..StateStore::MAX_SUPERSEDED_LAUNCHES {
        store
            .record_superseded_launch(&format!("launch-{index}"))
            .unwrap();
    }
    assert!(store.record_superseded_launch("one-too-many").is_err());
    assert!(store.is_launch_superseded("launch-a").unwrap());
}

#[test]
fn allocator_exhaustion_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let watermark = root.path().join("allocator.watermark");
    std::fs::write(&watermark, format!("terminal={}\n", i64::MAX)).unwrap();
    let store = StateStore::open(&database, &watermark).unwrap();
    assert!(store.allocate(AllocationKind::Terminal).is_err());
}
