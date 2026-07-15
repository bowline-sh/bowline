use super::common::*;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentNamespaceEntryRecord {
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SnapshotId,
    pub project_id: Option<ProjectId>,
    pub component_prefix: WorkspaceRelativePath,
    pub path: WorkspaceRelativePath,
    pub kind: NamespaceEntryKind,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
    pub content_id: Option<ContentId>,
    pub content_layout_id: Option<ContentLayoutId>,
    pub symlink_target: Option<String>,
    pub byte_len: Option<u64>,
    pub executability: FileExecutability,
    pub hydration_state: HydrationState,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRebuildInput {
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SnapshotId,
    pub component_prefix: WorkspaceRelativePath,
    pub entries: Vec<CurrentNamespaceEntryRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionSlice {
    RootLevel,
    Component(WorkspaceRelativePath),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CurrentNamespaceReplaceReport {
    pub rows_deleted: u64,
    pub rows_written: u64,
}

impl MetadataStore {
    pub fn replace_current_namespace_projection_stream<E>(
        &mut self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
        slices: &[ProjectionSlice],
        produce: impl FnMut(
            &ProjectionSlice,
            &mut dyn FnMut(CurrentNamespaceEntryRecord) -> Result<(), E>,
        ) -> Result<(), E>,
    ) -> Result<CurrentNamespaceReplaceReport, E>
    where
        E: From<MetadataError>,
    {
        self.with_committed(|store| {
            store.replace_current_namespace_projection_stream_uncommitted(
                workspace_id,
                snapshot_id,
                slices,
                produce,
            )
        })
    }

    pub(crate) fn replace_current_namespace_projection_stream_uncommitted<E>(
        &mut self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
        slices: &[ProjectionSlice],
        mut produce: impl FnMut(
            &ProjectionSlice,
            &mut dyn FnMut(CurrentNamespaceEntryRecord) -> Result<(), E>,
        ) -> Result<(), E>,
    ) -> Result<CurrentNamespaceReplaceReport, E>
    where
        E: From<MetadataError>,
    {
        let completeness = self
            .snapshot_root_completeness(workspace_id, snapshot_id)
            .map_err(E::from)?;
        if let Some(missing) = completeness.missing_or_unverified.first() {
            return Err(E::from(MetadataError::IncompleteSnapshotRoot {
                snapshot_id: snapshot_id.clone(),
                logical_id: missing.logical_id.as_str().to_string(),
            }));
        }
        let unique = slices
            .iter()
            .map(|slice| match slice {
                ProjectionSlice::RootLevel => "root-level".to_string(),
                ProjectionSlice::Component(prefix) => format!("component:{}", prefix.as_str()),
            })
            .collect::<BTreeSet<_>>();
        if unique.len() != slices.len() {
            return Err(E::from(MetadataError::InvalidCurrentNamespaceProjection {
                field: "scope",
                reason: "projection replacement contains a duplicate slice",
            }));
        }
        let mut report = CurrentNamespaceReplaceReport::default();
        for slice in slices {
            report.rows_deleted = report.rows_deleted.saturating_add(match slice {
                ProjectionSlice::RootLevel => self
                    .connection
                    .execute(
                        "DELETE FROM current_namespace_entries
                         WHERE workspace_id = ?1 AND instr(path, '/') = 0",
                        [workspace_id.as_str()],
                    )
                    .map_err(MetadataError::from)
                    .map_err(E::from)? as u64,
                ProjectionSlice::Component(prefix) => {
                    delete_projection_component(self, workspace_id, prefix)?
                }
            });
            let mut previous = None::<String>;
            let mut sink = |record: CurrentNamespaceEntryRecord| -> Result<(), E> {
                validate_streamed_projection_record(
                    workspace_id,
                    snapshot_id,
                    slice,
                    previous.as_deref(),
                    &record,
                )
                .map_err(E::from)?;
                previous = Some(record.path.as_str().to_string());
                insert_projection_record(self, &record).map_err(E::from)?;
                report.rows_written = report.rows_written.saturating_add(1);
                Ok(())
            };
            produce(slice, &mut sink)?;
        }
        Ok(report)
    }

    pub fn rebuild_current_namespace_projection(
        &mut self,
        input: &ProjectionRebuildInput,
    ) -> Result<CurrentNamespaceReplaceReport, MetadataError> {
        self.replace_current_namespace_owned_projection(
            &input.workspace_id,
            &input.snapshot_id,
            None,
            std::slice::from_ref(input),
        )
    }

    /// Replaces every projection slice the caller observed in one transaction.
    /// `Some(root_level_entries)` owns the complete root-level slice, including
    /// the empty slice; `None` preserves all root-level rows outside components.
    pub fn replace_current_namespace_owned_projection(
        &mut self,
        workspace_id: &WorkspaceId,
        snapshot_id: &SnapshotId,
        root_level_entries: Option<&[CurrentNamespaceEntryRecord]>,
        component_rebuilds: &[ProjectionRebuildInput],
    ) -> Result<CurrentNamespaceReplaceReport, MetadataError> {
        let completeness = self.snapshot_root_completeness(workspace_id, snapshot_id)?;
        if let Some(missing) = completeness.missing_or_unverified.first() {
            return Err(MetadataError::IncompleteSnapshotRoot {
                snapshot_id: snapshot_id.clone(),
                logical_id: missing.logical_id.as_str().to_string(),
            });
        }
        if let Some(entries) = root_level_entries {
            validate_root_level_projection(workspace_id, snapshot_id, entries)?;
        }
        for input in component_rebuilds {
            if &input.workspace_id != workspace_id || &input.snapshot_id != snapshot_id {
                return Err(MetadataError::InvalidCurrentNamespaceProjection {
                    field: "identity",
                    reason: "component rebuild does not match owned projection",
                });
            }
            validate_projection_input(input)?;
        }
        self.with_committed(|store| {
            let mut rows_deleted = 0_u64;
            let mut rows_written = 0_u64;
            if let Some(entries) = root_level_entries {
                rows_deleted = store.connection.execute(
                    "DELETE FROM current_namespace_entries
                     WHERE workspace_id = ?1 AND instr(path, '/') = 0",
                    [workspace_id.as_str()],
                )? as u64;
                for record in entries {
                    insert_projection_record(store, record)?;
                }
                rows_written = entries.len() as u64;
            }
            for input in component_rebuilds {
                rows_deleted = rows_deleted.saturating_add(delete_projection_component(
                    store,
                    &input.workspace_id,
                    &input.component_prefix,
                )?);
                for record in &input.entries {
                    insert_projection_record(store, record)?;
                }
                rows_written = rows_written.saturating_add(input.entries.len() as u64);
            }
            Ok(CurrentNamespaceReplaceReport {
                rows_deleted,
                rows_written,
            })
        })
    }

    pub fn current_namespace_entry(
        &self,
        workspace_id: &WorkspaceId,
        path: &WorkspaceRelativePath,
    ) -> Result<Option<CurrentNamespaceEntryRecord>, MetadataError> {
        self.connection
            .query_row(
                &format!(
                    "{} WHERE workspace_id = ?1 AND path = ?2",
                    current_namespace_select()
                ),
                params![workspace_id.as_str(), path.as_str()],
                current_namespace_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn current_namespace_entries_by_component_prefix(
        &self,
        workspace_id: &WorkspaceId,
        prefix: &WorkspaceRelativePath,
        limit: u64,
    ) -> Result<Vec<CurrentNamespaceEntryRecord>, MetadataError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut statement = if prefix.is_empty() {
            self.connection.prepare(&format!(
                "{} WHERE workspace_id = ?1 ORDER BY path LIMIT ?2",
                current_namespace_select()
            ))?
        } else {
            self.connection.prepare(&format!(
                "{} WHERE workspace_id = ?1 AND (path = ?2 OR path LIKE ?3 ESCAPE '\\')
                 ORDER BY path LIMIT ?4",
                current_namespace_select()
            ))?
        };
        let rows = if prefix.is_empty() {
            statement.query_map(
                params![workspace_id.as_str(), sql_limit(Some(limit))],
                current_namespace_from_row,
            )?
        } else {
            let descendants = format!("{}/%", escape_like(prefix.as_str()));
            statement.query_map(
                params![
                    workspace_id.as_str(),
                    prefix.as_str(),
                    descendants,
                    sql_limit(Some(limit)),
                ],
                current_namespace_from_row,
            )?
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn validate_streamed_projection_record(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    slice: &ProjectionSlice,
    previous: Option<&str>,
    record: &CurrentNamespaceEntryRecord,
) -> Result<(), MetadataError> {
    if &record.workspace_id != workspace_id || &record.snapshot_id != snapshot_id {
        return Err(MetadataError::InvalidCurrentNamespaceProjection {
            field: "identity",
            reason: "streamed entry does not match projection identity",
        });
    }
    let in_scope = match slice {
        ProjectionSlice::RootLevel => {
            !record.path.is_empty()
                && !record.path.as_str().contains('/')
                && record.component_prefix == record.path
        }
        ProjectionSlice::Component(prefix) => {
            record.component_prefix == *prefix && record.path.is_equal_to_or_below(prefix)
        }
    };
    if !in_scope {
        return Err(MetadataError::InvalidCurrentNamespaceProjection {
            field: "path",
            reason: "streamed entry is outside its projection slice",
        });
    }
    if previous.is_some_and(|path| path >= record.path.as_str()) {
        return Err(MetadataError::InvalidCurrentNamespaceProjection {
            field: "path",
            reason: "streamed entries must be strictly ordered and unique",
        });
    }
    Ok(())
}

fn delete_projection_component(
    store: &MetadataStore,
    workspace_id: &WorkspaceId,
    component_prefix: &WorkspaceRelativePath,
) -> Result<u64, MetadataError> {
    let component = component_prefix.as_str();
    let deleted = if component.is_empty() {
        store.connection.execute(
            "DELETE FROM current_namespace_entries WHERE workspace_id = ?1",
            [workspace_id.as_str()],
        )?
    } else {
        let descendants = format!("{}/%", escape_like(component));
        store.connection.execute(
            "DELETE FROM current_namespace_entries
             WHERE workspace_id = ?1 AND (path = ?2 OR path LIKE ?3 ESCAPE '\\')",
            params![workspace_id.as_str(), component, descendants],
        )?
    };
    Ok(deleted as u64)
}

fn validate_root_level_projection(
    workspace_id: &WorkspaceId,
    snapshot_id: &SnapshotId,
    entries: &[CurrentNamespaceEntryRecord],
) -> Result<(), MetadataError> {
    let mut previous: Option<&str> = None;
    for entry in entries {
        if &entry.workspace_id != workspace_id || &entry.snapshot_id != snapshot_id {
            return Err(MetadataError::InvalidCurrentNamespaceProjection {
                field: "identity",
                reason: "root-level entry does not match owned projection",
            });
        }
        if entry.path.is_empty()
            || entry.path.as_str().contains('/')
            || entry.component_prefix != entry.path
        {
            return Err(MetadataError::InvalidCurrentNamespaceProjection {
                field: "path",
                reason: "root-level replacement contains a non-root path",
            });
        }
        if previous.is_some_and(|previous| previous >= entry.path.as_str()) {
            return Err(MetadataError::InvalidCurrentNamespaceProjection {
                field: "path",
                reason: "root-level entries must be strictly ordered and unique",
            });
        }
        previous = Some(entry.path.as_str());
    }
    Ok(())
}

fn validate_projection_input(input: &ProjectionRebuildInput) -> Result<(), MetadataError> {
    let mut previous: Option<&str> = None;
    for entry in &input.entries {
        if entry.workspace_id != input.workspace_id
            || entry.snapshot_id != input.snapshot_id
            || entry.component_prefix != input.component_prefix
        {
            return Err(MetadataError::InvalidCurrentNamespaceProjection {
                field: "identity",
                reason: "projection entry does not match rebuild ownership",
            });
        }
        if !entry.path.is_equal_to_or_below(&input.component_prefix) {
            return Err(MetadataError::InvalidCurrentNamespaceProjection {
                field: "path",
                reason: "projection entry is outside the component prefix",
            });
        }
        if let Some(previous) = previous
            && previous >= entry.path.as_str()
        {
            return Err(MetadataError::InvalidCurrentNamespaceProjection {
                field: "path",
                reason: "projection entries must be strictly ordered and unique",
            });
        }
        previous = Some(entry.path.as_str());
    }
    Ok(())
}

fn insert_projection_record(
    store: &MetadataStore,
    record: &CurrentNamespaceEntryRecord,
) -> Result<(), MetadataError> {
    store.connection.execute(
        "INSERT INTO current_namespace_entries
         (workspace_id, snapshot_id, project_id, component_prefix, path, kind,
          classification, mode, access_json, content_id, content_layout_id, symlink_target,
          byte_len, executability, hydration_state, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            record.workspace_id.as_str(),
            record.snapshot_id.as_str(),
            record.project_id.as_ref().map(ProjectId::as_str),
            record.component_prefix.as_str(),
            record.path.as_str(),
            serialize_json_variant(&record.kind)?,
            serialize_json_variant(&record.classification)?,
            serialize_json_variant(&record.mode)?,
            serde_json::to_string(&record.access)
                .map_err(|error| MetadataError::Sqlite(json_to_sql_error(error)))?,
            record.content_id.as_ref().map(ContentId::as_str),
            record
                .content_layout_id
                .as_ref()
                .map(ContentLayoutId::as_str),
            record.symlink_target,
            record.byte_len,
            serialize_json_variant(&record.executability)?,
            serialize_json_variant(&record.hydration_state)?,
            record.updated_at,
        ],
    )?;
    Ok(())
}

fn current_namespace_select() -> &'static str {
    "SELECT workspace_id, snapshot_id, project_id, component_prefix, path, kind,
            classification, mode, access_json, content_id, content_layout_id, symlink_target,
            byte_len, executability, hydration_state, updated_at
     FROM current_namespace_entries"
}

fn current_namespace_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<CurrentNamespaceEntryRecord, rusqlite::Error> {
    Ok(CurrentNamespaceEntryRecord {
        workspace_id: WorkspaceId::new(row.get::<_, String>(0)?),
        snapshot_id: SnapshotId::new(row.get::<_, String>(1)?),
        project_id: row.get::<_, Option<String>>(2)?.map(ProjectId::new),
        component_prefix: WorkspaceRelativePath::new(row.get::<_, String>(3)?),
        path: WorkspaceRelativePath::new(row.get::<_, String>(4)?),
        kind: deserialize_json_variant(row.get::<_, String>(5)?)?,
        classification: deserialize_json_variant(row.get::<_, String>(6)?)?,
        mode: deserialize_json_variant(row.get::<_, String>(7)?)?,
        access: serde_json::from_str(&row.get::<_, String>(8)?).map_err(json_to_sql_read_error)?,
        content_id: row.get::<_, Option<String>>(9)?.map(ContentId::new),
        content_layout_id: row.get::<_, Option<String>>(10)?.map(ContentLayoutId::new),
        symlink_target: row.get(11)?,
        byte_len: row.get(12)?,
        executability: deserialize_json_variant(row.get::<_, String>(13)?)?,
        hydration_state: deserialize_json_variant(row.get::<_, String>(14)?)?,
        updated_at: row.get(15)?,
    })
}
