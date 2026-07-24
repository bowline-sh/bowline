use super::common::*;
use super::*;

impl MetadataStore {
    pub(crate) fn with_classified_transaction<T, E>(
        &mut self,
        operation: impl FnOnce(&mut MetadataStore) -> Result<T, E>,
    ) -> Result<T, super::ClassifiedTransactionError<E>>
    where
        E: From<MetadataError>,
    {
        if !self.connection.is_autocommit() {
            return operation(self).map_err(super::ClassifiedTransactionError::BeforeCommit);
        }
        self.connection
            .execute("BEGIN IMMEDIATE", [])
            .map_err(MetadataError::from)
            .map_err(E::from)
            .map_err(super::ClassifiedTransactionError::BeforeCommit)?;
        let value = match operation(self) {
            Ok(value) => value,
            Err(error) => {
                if let Err(rollback_error) = self.connection.execute("ROLLBACK", []) {
                    eprintln!("bowline metadata transaction rollback failed: {rollback_error}");
                }
                return Err(super::ClassifiedTransactionError::BeforeCommit(error));
            }
        };
        if let Err(error) = self.connection.execute("COMMIT", []) {
            if let Err(rollback_error) = self.connection.execute("ROLLBACK", []) {
                eprintln!("bowline metadata transaction rollback failed: {rollback_error}");
            }
            return Err(super::ClassifiedTransactionError::CommitAcknowledgement(
                MetadataError::from(error),
            ));
        }
        Ok(value)
    }

    pub(crate) fn data_version(&self) -> Result<u64, MetadataError> {
        self.connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self, MetadataError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut connection = Connection::open(&path)?;
        configure_connection(&connection)?;
        initialize_schema(&mut connection)?;

        Ok(Self { connection })
    }

    pub fn inspect(path: impl Into<PathBuf>) -> DatabaseInspection {
        let path = path.into();
        if !path.exists() {
            return DatabaseInspection {
                state: DatabaseState::Missing,
                path,
            };
        }

        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return DatabaseInspection {
                    state: DatabaseState::Missing,
                    path,
                };
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return DatabaseInspection {
                    state: DatabaseState::PermissionDenied,
                    path,
                };
            }
            Err(_) => {
                return DatabaseInspection {
                    state: DatabaseState::Corrupt,
                    path,
                };
            }
        };

        if metadata.len() == 0 {
            return DatabaseInspection {
                state: DatabaseState::Empty,
                path,
            };
        }

        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let state = match Connection::open_with_flags(&path, flags) {
            Ok(connection) => match configure_read_only_connection(
                &connection,
                MetadataReadRole::SchemaInspection,
            ) {
                Ok(()) => inspect_open_connection(&connection),
                Err(MetadataError::Sqlite(error)) => classify_open_error(&error),
                Err(_) => DatabaseState::Corrupt,
            },
            Err(error) => classify_open_error(&error),
        };

        DatabaseInspection { state, path }
    }

    pub fn journal_mode(&self) -> Result<String, MetadataError> {
        Ok(self
            .connection
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?)
    }

    pub fn has_table(&self, table: &str) -> Result<bool, MetadataError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn assert_schema_tables(&self) -> Result<(), MetadataError> {
        for table in TABLES {
            if !self.has_table(table)? {
                return Err(MetadataError::Sqlite(rusqlite::Error::InvalidQuery));
            }
        }
        Ok(())
    }

    pub(crate) fn with_committed<T, E>(
        &mut self,
        f: impl FnOnce(&mut MetadataStore) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<MetadataError>,
    {
        match self.with_classified_transaction(f) {
            Ok(value) => Ok(value),
            Err(super::ClassifiedTransactionError::BeforeCommit(error)) => Err(error),
            Err(super::ClassifiedTransactionError::CommitAcknowledgement(error)) => {
                Err(error.into())
            }
        }
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}
