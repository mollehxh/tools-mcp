use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AllocationKind {
    Generation,
    Terminal,
    Opaque,
}

impl AllocationKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Generation => "generation",
            Self::Terminal => "terminal",
            Self::Opaque => "opaque",
        }
    }
    const fn initial(self) -> u64 {
        match self {
            Self::Generation | Self::Opaque => 1,
            Self::Terminal => 999,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateStoreError {
    #[error("gateway SQLite state failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("gateway allocation watermark failed")]
    Watermark(#[from] std::io::Error),
    #[error("gateway allocation watermark is malformed")]
    MalformedWatermark,
    #[error("gateway allocation watermark is missing for existing state")]
    MissingWatermark,
    #[error("gateway allocation space is exhausted")]
    Exhausted,
    #[error("gateway superseded-launch tombstone capacity is exhausted")]
    TombstoneCapacity,
    #[error("gateway launch identity is invalid")]
    InvalidLaunchIdentity,
}

pub struct StateStore {
    connection: Mutex<Connection>,
    watermark_path: PathBuf,
}

impl StateStore {
    pub const MAX_SUPERSEDED_LAUNCHES: usize = 4_096;
    const MAX_LAUNCH_ID_BYTES: usize = 256;

    /// Opens durable `SQLite` state and its rollback-excluded allocation watermark.
    ///
    /// # Errors
    ///
    /// Returns an error if storage cannot be created, configured, or validated.
    pub fn open(database_path: &Path, watermark_path: &Path) -> Result<Self, StateStoreError> {
        let database_existed = database_path.exists();
        if let Some(parent) = database_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = watermark_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if database_existed && !watermark_path.exists() {
            return Err(StateStoreError::MissingWatermark);
        }
        if watermark_path.exists() {
            read_watermarks(watermark_path)?;
        }
        let connection = Connection::open(database_path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS allocations (
                kind TEXT PRIMARY KEY,
                value INTEGER NOT NULL CHECK(value >= 0)
             );
             CREATE TABLE IF NOT EXISTS superseded_launches (
                launch_instance_id TEXT PRIMARY KEY NOT NULL
             );",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
            watermark_path: watermark_path.to_path_buf(),
        })
    }

    /// Allocates a value above both `SQLite` and the rollback-excluded watermark.
    ///
    /// # Errors
    ///
    /// Returns an error on storage failure, malformed watermark, or exhaustion.
    pub fn allocate(&self, kind: AllocationKind) -> Result<u64, StateStoreError> {
        let mut connection = self.lock_connection();
        let database_value = connection
            .query_row(
                "SELECT value FROM allocations WHERE kind = ?1",
                [kind.name()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| StateStoreError::MalformedWatermark)?
            .unwrap_or_else(|| kind.initial());
        let mut watermarks = read_watermarks(&self.watermark_path)?;
        let watermark_value = watermarks
            .get(kind.name())
            .copied()
            .unwrap_or_else(|| kind.initial());
        let next = database_value
            .max(watermark_value)
            .checked_add(1)
            .ok_or(StateStoreError::Exhausted)?;
        let next_sql = i64::try_from(next).map_err(|_| StateStoreError::Exhausted)?;
        watermarks.insert(kind.name().to_owned(), next);
        write_watermarks(&self.watermark_path, &watermarks)?;
        let transaction = connection.transaction()?;
        transaction.execute("INSERT INTO allocations(kind, value) VALUES (?1, ?2) ON CONFLICT(kind) DO UPDATE SET value = excluded.value", params![kind.name(), next_sql])?;
        transaction.commit()?;
        Ok(next)
    }

    /// Records a launch identity as permanently superseded.
    ///
    /// The table is deliberately bounded and fails closed instead of evicting an old identity
    /// that could later reconnect. Re-recording an existing identity is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identities, storage failures, or exhausted capacity.
    pub fn record_superseded_launch(
        &self,
        launch_instance_id: &str,
    ) -> Result<(), StateStoreError> {
        Self::validate_launch_identity(launch_instance_id)?;
        let mut connection = self.lock_connection();
        let transaction = connection.transaction()?;
        let exists = transaction
            .query_row(
                "SELECT 1 FROM superseded_launches WHERE launch_instance_id = ?1",
                [launch_instance_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            transaction.commit()?;
            return Ok(());
        }
        let count =
            transaction.query_row("SELECT COUNT(*) FROM superseded_launches", [], |row| {
                row.get::<_, i64>(0)
            })?;
        if usize::try_from(count).map_or(true, |count| count >= Self::MAX_SUPERSEDED_LAUNCHES) {
            return Err(StateStoreError::TombstoneCapacity);
        }
        transaction.execute(
            "INSERT INTO superseded_launches(launch_instance_id) VALUES (?1)",
            [launch_instance_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Checks the durable supersession set.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identities or storage failures.
    pub fn is_launch_superseded(&self, launch_instance_id: &str) -> Result<bool, StateStoreError> {
        Self::validate_launch_identity(launch_instance_id)?;
        let connection = self.lock_connection();
        Ok(connection
            .query_row(
                "SELECT 1 FROM superseded_launches WHERE launch_instance_id = ?1",
                [launch_instance_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn validate_launch_identity(launch_instance_id: &str) -> Result<(), StateStoreError> {
        if launch_instance_id.is_empty()
            || launch_instance_id.len() > Self::MAX_LAUNCH_ID_BYTES
            || launch_instance_id.chars().any(char::is_control)
        {
            return Err(StateStoreError::InvalidLaunchIdentity);
        }
        Ok(())
    }

    fn lock_connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn read_watermarks(path: &Path) -> Result<BTreeMap<String, u64>, StateStoreError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    contents
        .lines()
        .map(|line| {
            let (kind, value) = line
                .split_once('=')
                .ok_or(StateStoreError::MalformedWatermark)?;
            let value = value
                .parse()
                .map_err(|_| StateStoreError::MalformedWatermark)?;
            Ok((kind.to_owned(), value))
        })
        .collect()
}

fn write_watermarks(path: &Path, watermarks: &BTreeMap<String, u64>) -> Result<(), std::io::Error> {
    let temporary = path.with_extension("next");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    for (kind, value) in watermarks {
        writeln!(file, "{kind}={value}")?;
    }
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}
