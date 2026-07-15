use rusqlite::Connection;

use super::store::MetadataError;

pub(super) fn current_schema_version(connection: &Connection) -> Result<u32, MetadataError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
        .map_err(Into::into)
}
