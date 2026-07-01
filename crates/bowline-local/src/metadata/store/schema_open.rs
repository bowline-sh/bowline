use super::common::*;
use super::*;

impl MetadataStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, MetadataError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let connection = Connection::open(&path)?;
        configure_connection(&connection)?;
        initialize_schema(&connection)?;

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
            Ok(connection) => inspect_open_connection(&connection),
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

    pub fn with_transaction<T>(
        &mut self,
        f: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, MetadataError> {
        let transaction = self.connection.transaction()?;
        let result = f(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}
