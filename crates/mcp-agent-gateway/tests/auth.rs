use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, SaltString};
use mcp_agent_gateway::{
    AuthConfig, AuthStore, AuthorizationGrant, DeviceAuthority, PendingDeviceEnrollment,
};
use rusqlite::{Connection, params};
use sha2::{Digest as _, Sha256};
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn config() -> AuthConfig {
    let salt = SaltString::encode_b64(b"0123456789abcdef").unwrap();
    let owner_secret_phc = Argon2::default()
        .hash_password(b"correct horse battery staple", &salt)
        .unwrap()
        .to_string();
    AuthConfig {
        client_id: "https://chatgpt.com/oauth/test/client.json".to_owned(),
        resource: "https://example.ngrok-free.dev/mcp".to_owned(),
        owner_secret_phc,
        token_hash_key: vec![7; 32],
        access_lifetime: Duration::from_mins(1),
        refresh_lifetime: Duration::from_hours(1),
    }
}

#[test]
fn grants_survive_restart_rotate_once_and_store_no_plaintext_tokens() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let config = config();
    let first = {
        let store = AuthStore::open(&database, config.clone()).unwrap();
        assert!(store.verify_owner_secret("correct horse battery staple"));
        assert!(!store.verify_owner_secret("wrong"));
        let pair = store.issue("offline_access").unwrap();
        assert!(store.validate_access(&pair.access_token).unwrap());
        pair
    };
    let store = AuthStore::open(&database, config).unwrap();
    assert!(store.validate_access(&first.access_token).unwrap());
    let rotated = store.refresh(&first.refresh_token).unwrap();
    assert!(store.validate_access(&rotated.access_token).unwrap());
    assert!(store.refresh(&first.refresh_token).is_err());
    assert!(store.refresh(&rotated.refresh_token).is_err());
    let database_bytes = std::fs::read(&database).unwrap();
    assert!(
        !database_bytes
            .windows(first.access_token.len())
            .any(|window| window == first.access_token.as_bytes())
    );
    assert!(
        !database_bytes
            .windows(first.refresh_token.len())
            .any(|window| window == first.refresh_token.as_bytes())
    );
}

#[test]
fn token_hashes_are_keyed_and_require_the_matched_secret_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let pair = {
        let store = AuthStore::open(&database, config()).unwrap();
        store.issue("offline_access").unwrap()
    };
    let legacy_access_hash = Sha256::digest(pair.access_token.as_bytes()).to_vec();
    let stored_access_hash = Connection::open(&database)
        .unwrap()
        .query_row("SELECT token_hash FROM oauth_access", [], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .unwrap();
    assert_ne!(stored_access_hash, legacy_access_hash);

    let mut wrong_key = config();
    wrong_key.token_hash_key = vec![8; 32];
    let mismatched = AuthStore::open(&database, wrong_key).unwrap();
    assert!(!mismatched.validate_access(&pair.access_token).unwrap());
    assert!(mismatched.refresh(&pair.refresh_token).is_err());

    let matched = AuthStore::open(&database, config()).unwrap();
    assert!(matched.validate_access(&pair.access_token).unwrap());
}

#[test]
fn pre_keyed_hash_tokens_upgrade_without_new_browser_consent() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    AuthStore::open(&database, config()).unwrap();
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let legacy_access = "legacy-access-token";
    let legacy_refresh = "legacy-refresh-token";
    let family = vec![3_u8; 32];
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO oauth_access(token_hash, client_id, resource, expires_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                Sha256::digest(legacy_access.as_bytes()).as_slice(),
                config().client_id,
                config().resource,
                now + 3600
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO oauth_refresh(token_hash, family_hash, client_id, resource, scope, expires_unix, consumed)
             VALUES (?1, ?2, ?3, ?4, 'offline_access', ?5, 0)",
            params![
                Sha256::digest(legacy_refresh.as_bytes()).as_slice(),
                family,
                config().client_id,
                config().resource,
                now + 3600
            ],
        )
        .unwrap();
    drop(connection);

    let store = AuthStore::open(&database, config()).unwrap();
    assert!(store.validate_access(legacy_access).unwrap());
    let rotated = store.refresh(legacy_refresh).unwrap();
    assert!(store.validate_access(&rotated.access_token).unwrap());
}

#[test]
fn access_authority_is_stable_across_refresh_and_grants_revoke_independently() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let store = AuthStore::open(&database, config()).unwrap();
    let first = store.issue("offline_access").unwrap();
    let first_authority = store.resolve_access(&first.access_token).unwrap().unwrap();
    let rotated = store.refresh(&first.refresh_token).unwrap();
    let rotated_authority = store
        .resolve_access(&rotated.access_token)
        .unwrap()
        .unwrap();
    assert_eq!(first_authority, rotated_authority);

    let unrelated = store.issue("offline_access").unwrap();
    let unrelated_authority = store
        .resolve_access(&unrelated.access_token)
        .unwrap()
        .unwrap();
    assert_ne!(first_authority.grant_id, unrelated_authority.grant_id);
    assert!(store.revoke_grant(&first_authority.grant_id).unwrap());
    assert!(store.resolve_access(&first.access_token).unwrap().is_none());
    assert!(
        store
            .resolve_access(&rotated.access_token)
            .unwrap()
            .is_none()
    );
    assert!(store.refresh(&rotated.refresh_token).is_err());
    assert_eq!(
        store.resolve_access(&unrelated.access_token).unwrap(),
        Some(unrelated_authority)
    );
}

#[test]
fn security_revision_advances_for_grants_tokens_and_device_authority() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let store = AuthStore::open(&database, config()).unwrap();
    let initial = store.security_revision().unwrap();
    let pair = store.issue("offline_access").unwrap();
    let issued = store.security_revision().unwrap();
    assert!(issued > initial);
    store.refresh(&pair.refresh_token).unwrap();
    let refreshed = store.security_revision().unwrap();
    assert!(refreshed > issued);
    store.revoke_all_devices().unwrap();
    let final_revision = store.security_revision().unwrap();
    assert!(final_revision >= refreshed);
    drop(store);
    assert_eq!(
        AuthStore::open(&database, config())
            .unwrap()
            .security_revision()
            .unwrap(),
        final_revision
    );
}

#[test]
fn stale_restore_revocation_removes_all_grants() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let store = AuthStore::open(&database, config()).unwrap();
    let pair = store.issue("offline_access").unwrap();
    store.revoke_all().unwrap();
    assert!(!store.validate_access(&pair.access_token).unwrap());
    assert!(store.refresh(&pair.refresh_token).is_err());
}

#[test]
fn device_enrollment_rotation_and_revocation_are_durable() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let store = AuthStore::open(&database, config()).unwrap();
    let expiry = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        + 3600;
    let first = DeviceAuthority {
        owner_id: "owner-1".to_owned(),
        device_id: "laptop".to_owned(),
        platform: "windows".to_owned(),
        certificate_fingerprint: "a".repeat(64),
        expires_unix: expiry,
    };
    store.approve_device(&first).unwrap();
    assert_eq!(store.resolve_device(&"a".repeat(64)).unwrap(), Some(first));

    let rotated = DeviceAuthority {
        owner_id: "owner-1".to_owned(),
        device_id: "laptop".to_owned(),
        platform: "windows".to_owned(),
        certificate_fingerprint: "b".repeat(64),
        expires_unix: expiry,
    };
    store.approve_device(&rotated).unwrap();
    assert!(store.resolve_device(&"a".repeat(64)).unwrap().is_none());
    assert_eq!(
        store.resolve_device(&"b".repeat(64)).unwrap(),
        Some(rotated)
    );
    assert!(store.revoke_device("laptop").unwrap());
    drop(store);

    let reopened = AuthStore::open(&database, config()).unwrap();
    assert!(reopened.resolve_device(&"b".repeat(64)).unwrap().is_none());
    assert!(!reopened.revoke_device("laptop").unwrap());
}

#[test]
fn authorization_code_survives_restart_is_hashed_and_consumed_once() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let expiry = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        + 60;
    let raw_code = "authorization-code-that-must-never-be-persisted";
    let grant = AuthorizationGrant {
        client_id: config().client_id,
        redirect_uri: "https://chatgpt.com/connector/oauth/test".to_owned(),
        resource: config().resource,
        scope: "offline_access".to_owned(),
        code_challenge: "challenge".to_owned(),
        expires_unix: expiry,
    };
    AuthStore::open(&database, config())
        .unwrap()
        .store_authorization_code(raw_code, &grant)
        .unwrap();
    assert!(
        !std::fs::read(&database)
            .unwrap()
            .windows(raw_code.len())
            .any(|window| window == raw_code.as_bytes())
    );
    let reopened = AuthStore::open(&database, config()).unwrap();
    let pair = reopened
        .exchange_authorization_code(
            raw_code,
            &grant.client_id,
            &grant.redirect_uri,
            &grant.resource,
            &grant.code_challenge,
        )
        .unwrap();
    assert!(reopened.validate_access(&pair.access_token).unwrap());
    assert!(
        reopened
            .exchange_authorization_code(
                raw_code,
                &grant.client_id,
                &grant.redirect_uri,
                &grant.resource,
                &grant.code_challenge,
            )
            .is_err()
    );
}

#[test]
fn concurrent_code_exchange_has_exactly_one_winner() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let grant = AuthorizationGrant {
        client_id: config().client_id,
        redirect_uri: "https://chatgpt.com/connector/oauth/test".to_owned(),
        resource: config().resource,
        scope: "offline_access".to_owned(),
        code_challenge: "challenge".to_owned(),
        expires_unix: i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap()
            + 60,
    };
    AuthStore::open(&database, config())
        .unwrap()
        .store_authorization_code("one-time-code", &grant)
        .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let database = database.clone();
            let grant = grant.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let store = AuthStore::open(&database, config()).unwrap();
                barrier.wait();
                store.exchange_authorization_code(
                    "one-time-code",
                    &grant.client_id,
                    &grant.redirect_uri,
                    &grant.resource,
                    &grant.code_challenge,
                )
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
}

#[test]
fn concurrent_refresh_has_one_winner_then_replay_revokes_its_family() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let pair = AuthStore::open(&database, config())
        .unwrap()
        .issue("offline_access")
        .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let database = database.clone();
            let refresh = pair.refresh_token.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let store = AuthStore::open(&database, config()).unwrap();
                barrier.wait();
                store.refresh(&refresh)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    let store = AuthStore::open(&database, config()).unwrap();
    assert!(store.refresh(&winner.refresh_token).is_err());
    assert!(!store.validate_access(&winner.access_token).unwrap());
}

#[test]
fn pending_csr_is_exactly_bound_and_can_complete_only_once() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let store = AuthStore::open(&database, config()).unwrap();
    let expiry = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        + 3600;
    let pending = PendingDeviceEnrollment {
        owner_id: "owner-1".to_owned(),
        device_id: "laptop".to_owned(),
        platform: "macos".to_owned(),
        csr_fingerprint: "a".repeat(64),
        public_key_fingerprint: "b".repeat(64),
        expires_unix: expiry,
    };
    store.begin_device_enrollment(&pending).unwrap();
    store.begin_device_enrollment(&pending).unwrap();
    let mut substituted = pending.clone();
    substituted.public_key_fingerprint = "9".repeat(64);
    assert!(store.begin_device_enrollment(&substituted).is_err());
    let certificate = DeviceAuthority {
        owner_id: pending.owner_id.clone(),
        device_id: pending.device_id.clone(),
        platform: pending.platform.clone(),
        certificate_fingerprint: "c".repeat(64),
        expires_unix: expiry,
    };

    assert!(
        store
            .complete_device_enrollment(
                &"d".repeat(64),
                &pending.public_key_fingerprint,
                &certificate,
            )
            .is_err()
    );
    assert!(
        store
            .complete_device_enrollment(&pending.csr_fingerprint, &"e".repeat(64), &certificate,)
            .is_err()
    );
    store
        .complete_device_enrollment(
            &pending.csr_fingerprint,
            &pending.public_key_fingerprint,
            &certificate,
        )
        .unwrap();
    assert!(
        store
            .complete_device_enrollment(
                &pending.csr_fingerprint,
                &pending.public_key_fingerprint,
                &certificate,
            )
            .is_err()
    );
    assert_eq!(
        store
            .resolve_device(&certificate.certificate_fingerprint)
            .unwrap(),
        Some(certificate)
    );
}

#[test]
fn pending_csr_race_has_one_winner_and_survives_restart() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("gateway.sqlite3");
    let expiry = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        + 3600;
    let pending = PendingDeviceEnrollment {
        owner_id: "owner-1".to_owned(),
        device_id: "desktop".to_owned(),
        platform: "windows".to_owned(),
        csr_fingerprint: "1".repeat(64),
        public_key_fingerprint: "2".repeat(64),
        expires_unix: expiry,
    };
    AuthStore::open(&database, config())
        .unwrap()
        .begin_device_enrollment(&pending)
        .unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let outcomes = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for fingerprint in ["3".repeat(64), "4".repeat(64)] {
            let barrier = barrier.clone();
            let database = database.clone();
            let pending = pending.clone();
            handles.push(scope.spawn(move || {
                let store = AuthStore::open(&database, config()).unwrap();
                let certificate = DeviceAuthority {
                    owner_id: pending.owner_id.clone(),
                    device_id: pending.device_id.clone(),
                    platform: pending.platform.clone(),
                    certificate_fingerprint: fingerprint,
                    expires_unix: pending.expires_unix,
                };
                barrier.wait();
                store.complete_device_enrollment(
                    &pending.csr_fingerprint,
                    &pending.public_key_fingerprint,
                    &certificate,
                )
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);

    let reopened = AuthStore::open(&database, config()).unwrap();
    assert!(
        reopened
            .complete_device_enrollment(
                &pending.csr_fingerprint,
                &pending.public_key_fingerprint,
                &DeviceAuthority {
                    owner_id: pending.owner_id,
                    device_id: pending.device_id,
                    platform: pending.platform,
                    certificate_fingerprint: "5".repeat(64),
                    expires_unix: pending.expires_unix,
                },
            )
            .is_err()
    );
}
