use skill_store::{
    GitFetcher, GixGitFetcher, InstallLimits, RepositoryEntry, TransportHop, TransportScript,
    evaluate_transport_script, normalize_git_source, validate_pack_expansion,
    validate_repository_tree,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

#[test]
fn source_validation_rejects_credential_bearing_and_non_public_urls_without_echoing_secrets() {
    for source in [
        "https://user:top-secret@example.com/repo.git",
        "https://example.com/repo.git?token=top-secret",
        "http://example.com/repo.git",
        "https://example.com:8443/repo.git",
        "https://127.0.0.1/repo.git",
        "https://169.254.169.254/latest/meta-data",
    ] {
        let error = normalize_git_source(source, None, None).unwrap_err();
        assert!(!error.to_string().contains("top-secret"));
    }
}

#[test]
fn github_tree_urls_normalize_to_repository_revision_and_selector() {
    let source = normalize_git_source(
        "https://github.com/example/skills/tree/main/skills/demo",
        None,
        None,
    )
    .unwrap();
    assert_eq!(source.repository, "https://github.com/example/skills.git");
    assert_eq!(source.revision.as_deref(), Some("main"));
    assert_eq!(source.selector.as_deref(), Some("skills/demo"));
}

#[test]
fn github_tree_rejects_slash_branch_ambiguity_and_accepts_explicit_components() {
    assert!(
        normalize_git_source(
            "https://github.com/example/skills/tree/feature/demo/skills/demo",
            None,
            None,
        )
        .is_err()
    );
    let explicit = normalize_git_source(
        "https://github.com/example/skills.git",
        Some("skills/demo"),
        Some("feature/demo"),
    )
    .unwrap();
    assert_eq!(explicit.revision.as_deref(), Some("feature/demo"));
    assert_eq!(explicit.selector.as_deref(), Some("skills/demo"));
}

#[test]
fn tree_validation_rejects_nonportable_paths_links_submodules_and_case_collisions() {
    let cases = vec![
        vec![RepositoryEntry::regular("../SKILL.md", Vec::new())],
        vec![RepositoryEntry::regular("demo/CON.txt", Vec::new())],
        vec![RepositoryEntry::regular("demo/value:stream", Vec::new())],
        vec![RepositoryEntry::symlink("demo/link")],
        vec![RepositoryEntry::submodule("demo/vendor")],
        vec![
            RepositoryEntry::regular("demo/A.md", Vec::new()),
            RepositoryEntry::regular("demo/a.md", Vec::new()),
        ],
    ];

    for entries in cases {
        assert!(validate_repository_tree(&entries, &InstallLimits::default()).is_err());
    }
}

#[test]
fn tree_limits_are_enforced_before_materialization() {
    let limits = InstallLimits {
        max_files: 1,
        max_file_bytes: 4,
        max_materialized_bytes: 4,
        ..InstallLimits::default()
    };
    assert!(
        validate_repository_tree(
            &[
                RepositoryEntry::regular("SKILL.md", b"1234".to_vec()),
                RepositoryEntry::regular("extra.md", b"x".to_vec()),
            ],
            &limits,
        )
        .is_err()
    );
    assert!(
        validate_repository_tree(
            &[RepositoryEntry::regular("SKILL.md", b"12345".to_vec())],
            &limits,
        )
        .is_err()
    );
}

#[test]
fn controlled_transport_revalidates_every_redirect_and_connect_resolution() {
    let public = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
    let private = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let redirect = TransportScript::new(vec![
        TransportHop::redirect(
            "https://example.com/repo.git/info/refs",
            vec![public],
            vec![public],
            "https://127.0.0.1/repo.git/info/refs",
        ),
        TransportHop::success(
            "https://127.0.0.1/repo.git/info/refs",
            vec![private],
            vec![private],
            vec![b"never connected".to_vec()],
        ),
    ]);
    assert!(evaluate_transport_script(&redirect, &InstallLimits::default()).is_err());
    assert_eq!(redirect.connected_hops(), 1);

    for unsafe_redirect in [
        "https://example.com/repo.git/info/refs?service=git-upload-pack",
        "https://example.com/other.git/not-git",
        "http://example.com/repo.git/info/refs",
    ] {
        let script = TransportScript::new(vec![TransportHop::redirect(
            "https://example.com/repo.git/info/refs",
            vec![public],
            vec![public],
            unsafe_redirect,
        )]);
        assert!(evaluate_transport_script(&script, &InstallLimits::default()).is_err());
        assert_eq!(script.connected_hops(), 1);
    }

    let rebinding = TransportScript::new(vec![TransportHop::success(
        "https://example.com/repo.git/info/refs",
        vec![public],
        vec![private],
        vec![b"never connected".to_vec()],
    )]);
    assert!(evaluate_transport_script(&rebinding, &InstallLimits::default()).is_err());
    assert_eq!(rebinding.connected_hops(), 0);
}

#[test]
fn controlled_transport_rejects_all_special_use_address_classes() {
    let special = [
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(192, 0, 0, 8)),
        IpAddr::V4(Ipv4Addr::new(192, 88, 99, 1)),
        IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 2, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 1)),
        IpAddr::V6(Ipv6Addr::new(0x4000, 0, 0, 0, 0, 0, 0, 1)),
    ];
    for address in special {
        let script = TransportScript::new(vec![TransportHop::success(
            "https://example.com/repo.git/info/refs",
            vec![address],
            vec![address],
            vec![b"never connected".to_vec()],
        )]);
        assert!(evaluate_transport_script(&script, &InstallLimits::default()).is_err());
        assert_eq!(script.connected_hops(), 0);
    }

    for address in [
        IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
        IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
    ] {
        let script = TransportScript::new(vec![TransportHop::success(
            "https://example.com/repo.git/info/refs",
            vec![address],
            vec![address],
            vec![b"ok".to_vec()],
        )]);
        assert_eq!(
            evaluate_transport_script(&script, &InstallLimits::default()).unwrap(),
            b"ok"
        );
    }
}

#[test]
fn controlled_transport_enforces_active_deadline_and_stream_byte_limit() {
    let public = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
    let slow = TransportScript::new(vec![TransportHop::success_with_delays(
        "https://example.com/repo.git/info/refs",
        vec![public],
        vec![public],
        vec![(Duration::from_millis(30), b"late".to_vec())],
    )]);
    let timeout_limits = InstallLimits {
        timeout: Duration::from_millis(5),
        ..InstallLimits::default()
    };
    assert!(evaluate_transport_script(&slow, &timeout_limits).is_err());
    assert!(slow.bytes_read() < 4);

    let oversized = TransportScript::new(vec![TransportHop::success(
        "https://example.com/repo.git/info/refs",
        vec![public],
        vec![public],
        vec![b"1234".to_vec(), b"5678".to_vec()],
    )]);
    let byte_limits = InstallLimits {
        max_transport_bytes: 6,
        ..InstallLimits::default()
    };
    assert!(evaluate_transport_script(&oversized, &byte_limits).is_err());
    assert_eq!(oversized.bytes_read(), 6);
}

#[test]
fn expanded_object_budget_rejects_pathological_delta_expansion() {
    let limits = InstallLimits {
        max_object_bytes: 16,
        max_expanded_object_bytes: 24,
        ..InstallLimits::default()
    };
    assert!(skill_store::validate_object_expansion(&[8, 8, 9], &limits).is_err());
    assert!(skill_store::validate_object_expansion(&[17], &limits).is_err());
}

#[test]
fn pack_expansion_budget_counts_unselected_objects_and_delta_work() {
    let limits = InstallLimits {
        max_objects: 2,
        max_object_bytes: 16,
        max_expanded_object_bytes: 24,
        ..InstallLimits::default()
    };

    assert!(validate_pack_expansion(&[(8, 2), (8, 2), (9, 2)], &limits).is_err());
    assert!(validate_pack_expansion(&[(8, 5), (8, 5), (8, 5)], &limits).is_err());
}

#[test]
#[ignore = "requires public HTTPS network access"]
fn production_gix_fetcher_uses_the_controlled_https_path() {
    let source = normalize_git_source(
        "https://github.com/octocat/Hello-World.git",
        None,
        Some("master"),
    )
    .unwrap();
    let fetched = GixGitFetcher
        .fetch(&source, &InstallLimits::default())
        .unwrap();
    assert_eq!(fetched.repository, source.repository);
    assert_eq!(fetched.commit.len(), 40);
    assert!(!fetched.entries.is_empty());
}
