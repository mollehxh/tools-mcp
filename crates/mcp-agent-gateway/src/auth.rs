use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordVerifier};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac as _};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceAuthority {
    pub owner_id: String,
    pub device_id: String,
    pub platform: String,
    pub certificate_fingerprint: String,
    pub expires_unix: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingDeviceEnrollment {
    pub owner_id: String,
    pub device_id: String,
    pub platform: String,
    pub csr_fingerprint: String,
    pub public_key_fingerprint: String,
    pub expires_unix: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationGrant {
    pub client_id: String,
    pub redirect_uri: String,
    pub resource: String,
    pub scope: String,
    pub code_challenge: String,
    pub expires_unix: i64,
}

#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub client_id: String,
    pub resource: String,
    pub owner_secret_phc: String,
    pub token_hash_key: Vec<u8>,
    pub access_lifetime: Duration,
    pub refresh_lifetime: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    pub scope: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessAuthority {
    pub owner_id: String,
    pub grant_id: String,
}

impl AccessAuthority {
    #[must_use]
    pub fn principal_fingerprint(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(self.owner_id.as_bytes());
        digest.update([0]);
        digest.update(self.grant_id.as_bytes());
        digest
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut output, byte| {
                use std::fmt::Write as _;
                write!(output, "{byte:02x}").expect("writing to a String cannot fail");
                output
            })
    }
}

const LEGACY_GRANT_ID: &str = "legacy-owner-grant-v1";
const OWNER_ID: &str = "owner";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("OAuth state storage failed")]
    Storage(#[from] rusqlite::Error),
    #[error("OAuth owner-secret hash is invalid")]
    InvalidOwnerHash,
    #[error("OAuth owner authentication failed")]
    AccessDenied,
    #[error("OAuth grant is invalid, expired, replayed, or revoked")]
    InvalidGrant,
    #[error("OAuth clock is unavailable")]
    Clock,
}

pub struct AuthStore {
    connection: Mutex<Connection>,
    config: AuthConfig,
}

impl AuthStore {
    /// Opens durable hashed OAuth grant state.
    ///
    /// # Errors
    ///
    /// Returns an error when the database or owner-secret hash is invalid.
    #[allow(clippy::too_many_lines)] // The migration is kept as one atomic schema declaration.
    pub fn open(database_path: &Path, config: AuthConfig) -> Result<Self, AuthError> {
        PasswordHash::new(&config.owner_secret_phc).map_err(|_| AuthError::InvalidOwnerHash)?;
        if config.token_hash_key.len() < 32 {
            return Err(AuthError::InvalidOwnerHash);
        }
        let connection = Connection::open(database_path)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS oauth_access (
                token_hash BLOB PRIMARY KEY,
                client_id TEXT NOT NULL,
                resource TEXT NOT NULL,
                expires_unix INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS oauth_code (
                code_hash BLOB PRIMARY KEY,
                client_id TEXT NOT NULL,
                redirect_uri TEXT NOT NULL,
                resource TEXT NOT NULL,
                scope TEXT NOT NULL,
                code_challenge TEXT NOT NULL,
                expires_unix INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS oauth_refresh (
                token_hash BLOB PRIMARY KEY,
                family_hash BLOB NOT NULL,
                client_id TEXT NOT NULL,
                resource TEXT NOT NULL,
                scope TEXT NOT NULL,
                expires_unix INTEGER NOT NULL,
                consumed INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS oauth_revoked_family (
                family_hash BLOB PRIMARY KEY,
                revoked_unix INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS relay_device (
                certificate_fingerprint TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL,
                device_id TEXT NOT NULL UNIQUE,
                platform TEXT NOT NULL,
                expires_unix INTEGER NOT NULL,
                revoked_unix INTEGER
             );
             CREATE TABLE IF NOT EXISTS relay_pending_enrollment (
                csr_fingerprint TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                platform TEXT NOT NULL,
                public_key_fingerprint TEXT NOT NULL,
                expires_unix INTEGER NOT NULL,
                consumed INTEGER NOT NULL DEFAULT 0
             );
             CREATE UNIQUE INDEX IF NOT EXISTS relay_one_pending_per_device
             ON relay_pending_enrollment(device_id) WHERE consumed = 0;
             CREATE TABLE IF NOT EXISTS oauth_grant (
                grant_id TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL,
                revoked_unix INTEGER
             );
             CREATE TABLE IF NOT EXISTS oauth_access_grant (
                token_hash BLOB PRIMARY KEY,
                grant_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS oauth_family_grant (
                family_hash BLOB PRIMARY KEY,
                grant_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS security_revision (
                singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                revision INTEGER NOT NULL CHECK(revision >= 0)
             );
             INSERT OR IGNORE INTO security_revision(singleton, revision) VALUES (1, 0);
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_code_insert AFTER INSERT ON oauth_code BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_code_update AFTER UPDATE ON oauth_code BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_code_delete AFTER DELETE ON oauth_code BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_access_insert AFTER INSERT ON oauth_access BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_access_update AFTER UPDATE ON oauth_access BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_access_delete AFTER DELETE ON oauth_access BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_refresh_insert AFTER INSERT ON oauth_refresh BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_refresh_update AFTER UPDATE ON oauth_refresh BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_refresh_delete AFTER DELETE ON oauth_refresh BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_family_insert AFTER INSERT ON oauth_revoked_family BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_family_update AFTER UPDATE ON oauth_revoked_family BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_family_delete AFTER DELETE ON oauth_revoked_family BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_grant_insert AFTER INSERT ON oauth_grant BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_grant_update AFTER UPDATE ON oauth_grant BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_oauth_grant_delete AFTER DELETE ON oauth_grant BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_relay_device_insert AFTER INSERT ON relay_device BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_relay_device_update AFTER UPDATE ON relay_device BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_relay_device_delete AFTER DELETE ON relay_device BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_relay_pending_insert AFTER INSERT ON relay_pending_enrollment BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_relay_pending_update AFTER UPDATE ON relay_pending_enrollment BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;
             CREATE TRIGGER IF NOT EXISTS security_revision_relay_pending_delete AFTER DELETE ON relay_pending_enrollment BEGIN UPDATE security_revision SET revision = revision + 1 WHERE singleton = 1; END;",
        )?;
        connection.execute(
            "INSERT OR IGNORE INTO oauth_grant(grant_id, owner_id, revoked_unix)
             VALUES (?1, ?2, NULL)",
            params![LEGACY_GRANT_ID, OWNER_ID],
        )?;
        connection.execute(
            "INSERT OR IGNORE INTO oauth_family_grant(family_hash, grant_id)
             SELECT DISTINCT family_hash, ?1 FROM oauth_refresh",
            [LEGACY_GRANT_ID],
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
            config,
        })
    }

    #[must_use]
    pub fn verify_owner_secret(&self, candidate: &str) -> bool {
        PasswordHash::new(&self.config.owner_secret_phc)
            .ok()
            .is_some_and(|hash| {
                Argon2::default()
                    .verify_password(candidate.as_bytes(), &hash)
                    .is_ok()
            })
    }

    /// Issues the first rotating token family after browser consent and PKCE validation.
    ///
    /// # Errors
    ///
    /// Returns an error on clock or durable storage failure.
    pub fn issue(&self, scope: &str) -> Result<TokenPair, AuthError> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let grant_id = random_token();
        transaction.execute(
            "INSERT INTO oauth_grant(grant_id, owner_id, revoked_unix) VALUES (?1, ?2, NULL)",
            params![grant_id, OWNER_ID],
        )?;
        let pair = issue_in_transaction(&transaction, &self.config, scope, &grant_id, None)?;
        transaction.commit()?;
        Ok(pair)
    }

    /// Stores a short-lived authorization code only as a one-way hash.
    ///
    /// # Errors
    ///
    /// Returns an error for an expired grant or storage failure.
    pub fn store_authorization_code(
        &self,
        code: &str,
        grant: &AuthorizationGrant,
    ) -> Result<(), AuthError> {
        if grant.expires_unix <= unix_seconds()? {
            return Err(AuthError::InvalidGrant);
        }
        self.lock().execute(
            "INSERT INTO oauth_code(code_hash, client_id, redirect_uri, resource, scope, code_challenge, expires_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                token_hash(&self.config, code).as_slice(),
                grant.client_id,
                grant.redirect_uri,
                grant.resource,
                grant.scope,
                grant.code_challenge,
                grant.expires_unix
            ],
        )?;
        Ok(())
    }

    /// Atomically consumes a bound code and creates its refresh family.
    ///
    /// # Errors
    ///
    /// Returns `InvalidGrant` for replay, expiry, or a binding mismatch.
    pub fn exchange_authorization_code(
        &self,
        code: &str,
        client_id: &str,
        redirect_uri: &str,
        resource: &str,
        code_challenge: &str,
    ) -> Result<TokenPair, AuthError> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let hash = token_hash(&self.config, code);
        let legacy_hash = legacy_token_hash(code);
        let grant = transaction
            .query_row(
                "SELECT code_hash, client_id, redirect_uri, resource, scope, code_challenge, expires_unix
                 FROM oauth_code WHERE code_hash = ?1 OR code_hash = ?2
                 ORDER BY CASE WHEN code_hash = ?1 THEN 0 ELSE 1 END LIMIT 1",
                params![hash.as_slice(), legacy_hash.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        AuthorizationGrant {
                            client_id: row.get(1)?,
                            redirect_uri: row.get(2)?,
                            resource: row.get(3)?,
                            scope: row.get(4)?,
                            code_challenge: row.get(5)?,
                            expires_unix: row.get(6)?,
                        },
                    ))
                },
            )
            .optional()?
            .ok_or(AuthError::InvalidGrant)?;
        transaction.execute(
            "DELETE FROM oauth_code WHERE code_hash = ?1",
            [grant.0.as_slice()],
        )?;
        let grant = grant.1;
        let valid = grant.expires_unix > unix_seconds()?
            && grant.client_id == client_id
            && grant.redirect_uri == redirect_uri
            && grant.resource == resource
            && grant.code_challenge == code_challenge;
        if !valid {
            transaction.commit()?;
            return Err(AuthError::InvalidGrant);
        }
        let grant_id = random_token();
        transaction.execute(
            "INSERT INTO oauth_grant(grant_id, owner_id, revoked_unix) VALUES (?1, ?2, NULL)",
            params![grant_id, OWNER_ID],
        )?;
        let pair = issue_in_transaction(&transaction, &self.config, &grant.scope, &grant_id, None)?;
        transaction.commit()?;
        Ok(pair)
    }

    /// Rotates a refresh token exactly once and revokes the family on replay.
    ///
    /// # Errors
    ///
    /// Returns `InvalidGrant` for missing, expired, replayed, or revoked grants.
    pub fn refresh(&self, refresh_token: &str) -> Result<TokenPair, AuthError> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let hash = token_hash(&self.config, refresh_token);
        let legacy_hash = legacy_token_hash(refresh_token);
        let grant = transaction
            .query_row(
                "SELECT token_hash, family_hash, client_id, resource, scope, expires_unix, consumed
             FROM oauth_refresh WHERE token_hash = ?1 OR token_hash = ?2
             ORDER BY CASE WHEN token_hash = ?1 THEN 0 ELSE 1 END LIMIT 1",
                params![hash.as_slice(), legacy_hash.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, bool>(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or(AuthError::InvalidGrant)?;
        let now = unix_seconds()?;
        let revoked = transaction
            .query_row(
                "SELECT 1 FROM oauth_revoked_family WHERE family_hash = ?1",
                [grant.1.as_slice()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let grant_id = transaction
            .query_row(
                "SELECT grant_id FROM oauth_family_grant WHERE family_hash = ?1",
                [grant.1.as_slice()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| LEGACY_GRANT_ID.to_owned());
        let grant_active = transaction
            .query_row(
                "SELECT 1 FROM oauth_grant WHERE grant_id = ?1 AND revoked_unix IS NULL",
                [&grant_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if grant.6 {
            transaction.execute("INSERT OR REPLACE INTO oauth_revoked_family(family_hash, revoked_unix) VALUES (?1, ?2)", params![grant.1.as_slice(), now])?;
            transaction.execute(
                "DELETE FROM oauth_refresh WHERE family_hash = ?1",
                [grant.1.as_slice()],
            )?;
            transaction.execute(
                "UPDATE oauth_grant SET revoked_unix = ?1
                 WHERE grant_id = ?2 AND revoked_unix IS NULL",
                params![now, grant_id],
            )?;
            transaction.execute(
                "DELETE FROM oauth_access WHERE token_hash IN (
                    SELECT token_hash FROM oauth_access_grant WHERE grant_id = ?1
                 )",
                [&grant_id],
            )?;
            transaction.commit()?;
            return Err(AuthError::InvalidGrant);
        }
        if revoked
            || !grant_active
            || grant.5 <= now
            || grant.2 != self.config.client_id
            || grant.3 != self.config.resource
        {
            return Err(AuthError::InvalidGrant);
        }
        transaction.execute(
            "UPDATE oauth_refresh SET consumed = 1 WHERE token_hash = ?1",
            [grant.0.as_slice()],
        )?;
        let pair = issue_in_transaction(
            &transaction,
            &self.config,
            &grant.4,
            &grant_id,
            Some(&grant.1),
        )?;
        transaction.commit()?;
        Ok(pair)
    }

    /// Validates a short-lived opaque access token without exposing it to logs.
    ///
    /// # Errors
    ///
    /// Returns an error only when the clock or state storage is unavailable.
    pub fn validate_access(&self, access_token: &str) -> Result<bool, AuthError> {
        Ok(self.resolve_access(access_token)?.is_some())
    }

    /// Resolves an access token to stable owner/grant authority.
    ///
    /// Existing pre-migration tokens share one legacy grant so a deployment does not force new
    /// browser consent; grants created after migration remain independently revocable.
    ///
    /// # Errors
    ///
    /// Returns an error only when the clock or state storage is unavailable.
    pub fn resolve_access(&self, access_token: &str) -> Result<Option<AccessAuthority>, AuthError> {
        let now = unix_seconds()?;
        let hash = token_hash(&self.config, access_token);
        let legacy_hash = legacy_token_hash(access_token);
        let connection = self.lock();
        let token = connection
            .query_row(
                "SELECT token_hash, client_id, resource, expires_unix FROM oauth_access
                 WHERE token_hash = ?1 OR token_hash = ?2
                 ORDER BY CASE WHEN token_hash = ?1 THEN 0 ELSE 1 END LIMIT 1",
                params![hash.as_slice(), legacy_hash.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some(token) = token.filter(|grant| {
            grant.1 == self.config.client_id && grant.2 == self.config.resource && grant.3 > now
        }) else {
            return Ok(None);
        };
        let grant_id = connection
            .query_row(
                "SELECT grant_id FROM oauth_access_grant WHERE token_hash = ?1",
                [token.0.as_slice()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| LEGACY_GRANT_ID.to_owned());
        let authority = connection
            .query_row(
                "SELECT owner_id FROM oauth_grant
                 WHERE grant_id = ?1 AND revoked_unix IS NULL",
                [&grant_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|owner_id| AccessAuthority { owner_id, grant_id });
        Ok(authority)
    }

    /// Revokes one OAuth grant without changing unrelated grants.
    ///
    /// # Errors
    ///
    /// Returns an error when revocation cannot commit atomically.
    pub fn revoke_grant(&self, grant_id: &str) -> Result<bool, AuthError> {
        if !valid_identity(grant_id) {
            return Err(AuthError::InvalidGrant);
        }
        let now = unix_seconds()?;
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE oauth_grant SET revoked_unix = ?1
             WHERE grant_id = ?2 AND revoked_unix IS NULL",
            params![now, grant_id],
        )? == 1;
        if changed {
            if grant_id == LEGACY_GRANT_ID {
                transaction.execute(
                    "DELETE FROM oauth_access
                     WHERE token_hash NOT IN (SELECT token_hash FROM oauth_access_grant)",
                    [],
                )?;
            }
            transaction.execute(
                "DELETE FROM oauth_access WHERE token_hash IN (
                    SELECT token_hash FROM oauth_access_grant WHERE grant_id = ?1
                 )",
                [grant_id],
            )?;
            transaction.execute(
                "DELETE FROM oauth_refresh WHERE family_hash IN (
                    SELECT family_hash FROM oauth_family_grant WHERE grant_id = ?1
                 )",
                [grant_id],
            )?;
        }
        transaction.commit()?;
        Ok(changed)
    }

    /// Returns the live in-process principal fingerprints for all unrevoked grants.
    ///
    /// # Errors
    ///
    /// Returns an error when grant state cannot be read consistently.
    pub fn active_principal_fingerprints(&self) -> Result<Vec<String>, AuthError> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT owner_id, grant_id FROM oauth_grant WHERE revoked_unix IS NULL")?;
        let authorities = statement.query_map([], |row| {
            Ok(AccessAuthority {
                owner_id: row.get(0)?,
                grant_id: row.get(1)?,
            })
        })?;
        authorities
            .map(|authority| authority.map(|value| value.principal_fingerprint()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(AuthError::from)
    }

    /// Returns the monotonic security-state revision used by quiesced recovery-set checks.
    ///
    /// # Errors
    ///
    /// Returns an error when the revision cannot be read.
    pub fn security_revision(&self) -> Result<u64, AuthError> {
        let revision = self.lock().query_row(
            "SELECT revision FROM security_revision WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        u64::try_from(revision).map_err(|_| AuthError::InvalidGrant)
    }

    /// Invalidates every token family after stale/disaster restore.
    ///
    /// # Errors
    ///
    /// Returns an error when the transaction cannot commit.
    pub fn revoke_all(&self) -> Result<(), AuthError> {
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM oauth_access", [])?;
        transaction.execute("DELETE FROM oauth_code", [])?;
        transaction.execute("DELETE FROM oauth_refresh", [])?;
        transaction.execute("DELETE FROM oauth_revoked_family", [])?;
        transaction.execute(
            "UPDATE oauth_grant SET revoked_unix = ?1 WHERE revoked_unix IS NULL",
            [unix_seconds()?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Records the exact CSR approved by a human-controlled enrollment flow.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, expired, duplicate, or conflicting requests.
    pub fn begin_device_enrollment(
        &self,
        enrollment: &PendingDeviceEnrollment,
    ) -> Result<(), AuthError> {
        if !valid_identity(&enrollment.owner_id)
            || !valid_identity(&enrollment.device_id)
            || !matches!(
                enrollment.platform.as_str(),
                "macos" | "windows" | "linux-vps"
            )
            || !valid_fingerprint(&enrollment.csr_fingerprint)
            || !valid_fingerprint(&enrollment.public_key_fingerprint)
            || enrollment.expires_unix <= unix_seconds()?
        {
            return Err(AuthError::InvalidGrant);
        }
        let connection = self.lock();
        connection.execute(
            "INSERT OR IGNORE INTO relay_pending_enrollment(
                csr_fingerprint, owner_id, device_id, platform,
                public_key_fingerprint, expires_unix, consumed
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![
                enrollment.csr_fingerprint,
                enrollment.owner_id,
                enrollment.device_id,
                enrollment.platform,
                enrollment.public_key_fingerprint,
                enrollment.expires_unix,
            ],
        )?;
        let exact = connection
            .query_row(
                "SELECT owner_id, device_id, platform, public_key_fingerprint, expires_unix
                 FROM relay_pending_enrollment
                 WHERE csr_fingerprint = ?1 AND consumed = 0",
                [&enrollment.csr_fingerprint],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?
            .is_some_and(|stored| {
                stored.0 == enrollment.owner_id
                    && stored.1 == enrollment.device_id
                    && stored.2 == enrollment.platform
                    && stored.3 == enrollment.public_key_fingerprint
                    && stored.4 == enrollment.expires_unix
            });
        if !exact {
            return Err(AuthError::InvalidGrant);
        }
        Ok(())
    }

    /// Atomically consumes one exact pending CSR and publishes its signed certificate identity.
    ///
    /// # Errors
    ///
    /// Returns `InvalidGrant` for substitution, replay, expiry, or changed enrollment fields.
    pub fn complete_device_enrollment(
        &self,
        csr_fingerprint: &str,
        public_key_fingerprint: &str,
        device: &DeviceAuthority,
    ) -> Result<(), AuthError> {
        if !valid_fingerprint(csr_fingerprint)
            || !valid_fingerprint(public_key_fingerprint)
            || !valid_device(device, unix_seconds()?)
        {
            return Err(AuthError::InvalidGrant);
        }
        let mut connection = self.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = transaction
            .query_row(
                "SELECT owner_id, device_id, platform, public_key_fingerprint, expires_unix
                 FROM relay_pending_enrollment
                 WHERE csr_fingerprint = ?1 AND consumed = 0",
                [csr_fingerprint],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or(AuthError::InvalidGrant)?;
        let now = unix_seconds()?;
        if pending.0 != device.owner_id
            || pending.1 != device.device_id
            || pending.2 != device.platform
            || pending.3 != public_key_fingerprint
            || pending.4 != device.expires_unix
            || pending.4 <= now
        {
            return Err(AuthError::InvalidGrant);
        }
        transaction.execute(
            "INSERT INTO relay_device(certificate_fingerprint, owner_id, device_id, platform, expires_unix, revoked_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)
             ON CONFLICT(device_id) DO UPDATE SET
               certificate_fingerprint = excluded.certificate_fingerprint,
               owner_id = excluded.owner_id,
               platform = excluded.platform,
               expires_unix = excluded.expires_unix,
               revoked_unix = NULL",
            params![
                device.certificate_fingerprint,
                device.owner_id,
                device.device_id,
                device.platform,
                device.expires_unix
            ],
        )?;
        if transaction.execute(
            "UPDATE relay_pending_enrollment SET consumed = 1
             WHERE csr_fingerprint = ?1 AND consumed = 0",
            [csr_fingerprint],
        )? != 1
        {
            return Err(AuthError::InvalidGrant);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Enrolls or rotates an SSH-approved device certificate record.
    ///
    /// # Errors
    ///
    /// Returns an error for empty identity fields, expired certificates, or storage failure.
    pub fn approve_device(&self, device: &DeviceAuthority) -> Result<(), AuthError> {
        let now = unix_seconds()?;
        if !valid_device(device, now) {
            return Err(AuthError::InvalidGrant);
        }
        self.lock().execute(
            "INSERT INTO relay_device(certificate_fingerprint, owner_id, device_id, platform, expires_unix, revoked_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)
             ON CONFLICT(device_id) DO UPDATE SET
               certificate_fingerprint = excluded.certificate_fingerprint,
               owner_id = excluded.owner_id,
               platform = excluded.platform,
               expires_unix = excluded.expires_unix,
               revoked_unix = NULL",
            params![
                device.certificate_fingerprint,
                device.owner_id,
                device.device_id,
                device.platform,
                device.expires_unix
            ],
        )?;
        Ok(())
    }

    /// Resolves only an active, unexpired device certificate.
    ///
    /// # Errors
    ///
    /// Returns an error when storage or the system clock is unavailable.
    pub fn resolve_device(
        &self,
        certificate_fingerprint: &str,
    ) -> Result<Option<DeviceAuthority>, AuthError> {
        let now = unix_seconds()?;
        let device = self
            .lock()
            .query_row(
                "SELECT owner_id, device_id, platform, certificate_fingerprint, expires_unix
                 FROM relay_device
                 WHERE certificate_fingerprint = ?1 AND revoked_unix IS NULL AND expires_unix > ?2",
                params![certificate_fingerprint, now],
                |row| {
                    Ok(DeviceAuthority {
                        owner_id: row.get(0)?,
                        device_id: row.get(1)?,
                        platform: row.get(2)?,
                        certificate_fingerprint: row.get(3)?,
                        expires_unix: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(device)
    }

    /// Revokes a device identity; active relay owners must close matching sockets immediately.
    ///
    /// # Errors
    ///
    /// Returns an error when storage or the system clock is unavailable.
    pub fn revoke_device(&self, device_id: &str) -> Result<bool, AuthError> {
        let now = unix_seconds()?;
        Ok(self.lock().execute(
            "UPDATE relay_device SET revoked_unix = ?1 WHERE device_id = ?2 AND revoked_unix IS NULL",
            params![now, device_id],
        )? == 1)
    }

    /// Revokes every enrolled relay identity after a stale/disaster restore.
    ///
    /// # Errors
    ///
    /// Returns an error when the revocation transaction cannot commit.
    pub fn revoke_all_devices(&self) -> Result<(), AuthError> {
        let now = unix_seconds()?;
        self.lock().execute(
            "UPDATE relay_device SET revoked_unix = ?1 WHERE revoked_unix IS NULL",
            [now],
        )?;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn valid_device(device: &DeviceAuthority, now: i64) -> bool {
    valid_identity(&device.owner_id)
        && valid_identity(&device.device_id)
        && matches!(device.platform.as_str(), "macos" | "windows" | "linux-vps")
        && valid_fingerprint(&device.certificate_fingerprint)
        && device.expires_unix > now
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl mcp_agent_relay::PeerResolver for AuthStore {
    fn resolve(&self, certificate_fingerprint: &str) -> Option<mcp_agent_relay::AuthenticatedPeer> {
        self.resolve_device(certificate_fingerprint)
            .ok()
            .flatten()
            .and_then(|device| {
                Some(mcp_agent_relay::AuthenticatedPeer {
                    owner_id: device.owner_id,
                    device_id: device.device_id,
                    certificate_fingerprint: device.certificate_fingerprint,
                    platform: match device.platform.as_str() {
                        "macos" => mcp_agent_relay::Platform::Macos,
                        "windows" => mcp_agent_relay::Platform::Windows,
                        "linux" | "linux-vps" => mcp_agent_relay::Platform::LinuxVps,
                        _ => return None,
                    },
                })
            })
    }
}

fn issue_in_transaction(
    transaction: &Transaction<'_>,
    config: &AuthConfig,
    scope: &str,
    grant_id: &str,
    family_hash: Option<&[u8]>,
) -> Result<TokenPair, AuthError> {
    let now = unix_seconds()?;
    let access_token = random_token();
    let refresh_token = random_token();
    let generated_family = token_hash(config, &random_token());
    let family_hash = family_hash.unwrap_or(&generated_family);
    let access_expiry = now
        .checked_add(i64::try_from(config.access_lifetime.as_secs()).map_err(|_| AuthError::Clock)?)
        .ok_or(AuthError::Clock)?;
    let refresh_expiry = now
        .checked_add(
            i64::try_from(config.refresh_lifetime.as_secs()).map_err(|_| AuthError::Clock)?,
        )
        .ok_or(AuthError::Clock)?;
    let access_hash = token_hash(config, &access_token);
    transaction.execute("INSERT INTO oauth_access(token_hash, client_id, resource, expires_unix) VALUES (?1, ?2, ?3, ?4)", params![access_hash.as_slice(), config.client_id, config.resource, access_expiry])?;
    transaction.execute(
        "INSERT INTO oauth_access_grant(token_hash, grant_id) VALUES (?1, ?2)",
        params![access_hash.as_slice(), grant_id],
    )?;
    transaction.execute("INSERT INTO oauth_refresh(token_hash, family_hash, client_id, resource, scope, expires_unix, consumed) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)", params![token_hash(config, &refresh_token).as_slice(), family_hash, config.client_id, config.resource, scope, refresh_expiry])?;
    transaction.execute(
        "INSERT OR IGNORE INTO oauth_family_grant(family_hash, grant_id) VALUES (?1, ?2)",
        params![family_hash, grant_id],
    )?;
    Ok(TokenPair {
        access_token,
        refresh_token,
        expires_in: config.access_lifetime.as_secs(),
        scope: scope.to_owned(),
    })
}

pub(crate) fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn legacy_token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}
fn token_hash(config: &AuthConfig, token: &str) -> [u8; 32] {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(&config.token_hash_key)
        .expect("HMAC-SHA256 accepts keys of every length");
    mac.update(token.as_bytes());
    mac.finalize().into_bytes().into()
}
fn unix_seconds() -> Result<i64, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AuthError::Clock)
        .and_then(|duration| i64::try_from(duration.as_secs()).map_err(|_| AuthError::Clock))
}
