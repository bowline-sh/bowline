use super::common::*;
use super::*;

#[derive(Debug, Clone)]
pub(crate) struct EnvRecordSourceReplacement {
    source_path: String,
    records: Vec<EnvRecord>,
}

impl EnvRecordSourceReplacement {
    pub(crate) fn new(source_path: String, records: Vec<EnvRecord>) -> Self {
        Self {
            source_path,
            records,
        }
    }
}

impl MetadataStore {
    pub(crate) fn commit_env_record_replacements(
        &mut self,
        workspace_id: &WorkspaceId,
        replacements: &[EnvRecordSourceReplacement],
    ) -> Result<(), MetadataError> {
        self.with_committed(|store| {
            store.apply_env_record_replacements_uncommitted(workspace_id, replacements)
        })
    }

    pub(crate) fn apply_env_record_replacements_uncommitted(
        &self,
        workspace_id: &WorkspaceId,
        replacements: &[EnvRecordSourceReplacement],
    ) -> Result<(), MetadataError> {
        for replacement in replacements {
            self.replace_env_records_for_source_uncommitted(
                workspace_id,
                &replacement.source_path,
                &replacement.records,
            )?;
        }
        Ok(())
    }

    pub fn replace_env_records_for_source(
        &mut self,
        workspace_id: &WorkspaceId,
        source_path: &str,
        records: &[EnvRecord],
    ) -> Result<(), MetadataError> {
        self.with_committed(|store| {
            store.replace_env_records_for_source_uncommitted(workspace_id, source_path, records)
        })
    }

    pub(crate) fn replace_env_records_for_source_uncommitted(
        &self,
        workspace_id: &WorkspaceId,
        source_path: &str,
        records: &[EnvRecord],
    ) -> Result<(), MetadataError> {
        let source_path = self.workspace_relative_path(workspace_id, source_path)?;
        let normalized_records = records
            .iter()
            .map(|record| {
                let mut record = record.clone();
                record.source_path =
                    self.workspace_relative_path(&record.workspace_id, &record.source_path)?;
                Ok(record)
            })
            .collect::<Result<Vec<_>, MetadataError>>()?;
        self.connection.execute(
            "DELETE FROM env_records
             WHERE workspace_id = ?1 AND source_path = ?2",
            params![workspace_id.as_str(), source_path],
        )?;
        for record in normalized_records {
            self.upsert_env_record(&record)?;
        }
        Ok(())
    }

    pub fn upsert_env_record(&self, record: &EnvRecord) -> Result<(), MetadataError> {
        let mut record = record.clone();
        record.source_path =
            self.workspace_relative_path(&record.workspace_id, &record.source_path)?;
        self.connection
            .execute(
                "INSERT INTO env_records
                 (id, workspace_id, project_id, source_path, key_name, access,
                  value_ciphertext_ref, updated_at, profile, occurrence_index, line_kind,
                  encrypted_locator_json, format_json, materialization_state, restriction_state,
                  key_epoch, metadata_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                 ON CONFLICT(id) DO UPDATE SET
                   workspace_id = excluded.workspace_id,
                   project_id = excluded.project_id,
                   source_path = excluded.source_path,
                   key_name = excluded.key_name,
                   access = excluded.access,
                   value_ciphertext_ref = excluded.value_ciphertext_ref,
                   updated_at = excluded.updated_at,
                   profile = excluded.profile,
                   occurrence_index = excluded.occurrence_index,
                   line_kind = excluded.line_kind,
                   encrypted_locator_json = excluded.encrypted_locator_json,
                   format_json = excluded.format_json,
                   materialization_state = excluded.materialization_state,
                   restriction_state = excluded.restriction_state,
                   key_epoch = excluded.key_epoch,
                   metadata_json = excluded.metadata_json",
                params![
                    record.id.as_str(),
                    record.workspace_id.as_str(),
                    record.project_id.as_ref().map(|id| id.as_str()),
                    record.source_path.as_str(),
                    record.key_name.as_str(),
                    serialize_access_flags(&record.access)?,
                    record.value_ciphertext_ref.as_deref(),
                    record.updated_at.as_str(),
                    record.profile.as_str(),
                    record.occurrence_index,
                    record.line_kind.as_str(),
                    record.encrypted_locator_json.as_str(),
                    record.format_json.as_str(),
                    record.materialization_state.as_str(),
                    record.restriction_state.as_str(),
                    record.key_epoch,
                    record.metadata_json.as_str(),
                ],
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn env_records(&self, workspace_id: &WorkspaceId) -> Result<Vec<EnvRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, source_path, key_name, access,
                    value_ciphertext_ref, updated_at, profile, occurrence_index, line_kind, encrypted_locator_json,
                    format_json, materialization_state, restriction_state, key_epoch, metadata_json
             FROM env_records
             WHERE workspace_id = ?1
             ORDER BY source_path, occurrence_index, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], env_record_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn env_records_for_source(
        &self,
        workspace_id: &WorkspaceId,
        source_path: &str,
    ) -> Result<Vec<EnvRecord>, MetadataError> {
        let source_path = self.workspace_relative_path(workspace_id, source_path)?;
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, source_path, key_name, access,
                    value_ciphertext_ref, updated_at, profile, occurrence_index, line_kind, encrypted_locator_json,
                    format_json, materialization_state, restriction_state, key_epoch, metadata_json
             FROM env_records
             WHERE workspace_id = ?1 AND source_path = ?2
             ORDER BY occurrence_index, id",
        )?;
        let rows = statement.query_map(
            params![workspace_id.as_str(), source_path],
            env_record_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn upsert_setup_receipt(&self, record: &SetupReceiptRecord) -> Result<(), MetadataError> {
        let cwd = self.workspace_relative_path(&record.workspace_id, &record.cwd)?;
        self.connection.execute(
            "INSERT INTO setup_receipts
             (id, workspace_id, project_id, command, state, receipt_json, updated_at,
              recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
              output_path, redacted_summary, setup_identity_hash, readiness_state,
              readiness_reason, readiness_remedy)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20)
             ON CONFLICT(id) DO UPDATE SET
               workspace_id = excluded.workspace_id,
               project_id = excluded.project_id,
               command = excluded.command,
               state = excluded.state,
               receipt_json = excluded.receipt_json,
               updated_at = excluded.updated_at,
               recipe_hash = excluded.recipe_hash,
               approval_state = excluded.approval_state,
               trigger = excluded.trigger,
               cwd = excluded.cwd,
               os = excluded.os,
               arch = excluded.arch,
               env_profile = excluded.env_profile,
               output_path = excluded.output_path,
               redacted_summary = excluded.redacted_summary,
               setup_identity_hash = excluded.setup_identity_hash,
               readiness_state = excluded.readiness_state,
               readiness_reason = excluded.readiness_reason,
               readiness_remedy = excluded.readiness_remedy",
            params![
                record.id.as_str(),
                record.workspace_id.as_str(),
                record.project_id.as_ref().map(|id| id.as_str()),
                record.command.as_str(),
                record.state.as_str(),
                record.receipt_json.as_str(),
                record.updated_at.as_str(),
                record.recipe_hash.as_str(),
                record.approval_state.as_str(),
                record.trigger.as_str(),
                cwd,
                record.os.as_str(),
                record.arch.as_str(),
                record.env_profile.as_str(),
                record.output_path.as_deref(),
                record.redacted_summary.as_str(),
                record.setup_identity_hash.as_str(),
                record.readiness_state.as_str(),
                record.readiness_reason.as_str(),
                record.readiness_remedy.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn setup_receipts(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<SetupReceiptRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, command, state, receipt_json, updated_at,
                    recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
                    output_path, redacted_summary, setup_identity_hash, readiness_state,
                    readiness_reason, readiness_remedy
             FROM setup_receipts
             WHERE workspace_id = ?1
             ORDER BY updated_at DESC, id",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], setup_receipt_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn setup_receipt_by_id(
        &self,
        workspace_id: &WorkspaceId,
        receipt_id: &str,
    ) -> Result<Option<SetupReceiptRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, project_id, command, state, receipt_json, updated_at,
                        recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
                        output_path, redacted_summary, setup_identity_hash, readiness_state,
                        readiness_reason, readiness_remedy
                 FROM setup_receipts
                 WHERE workspace_id = ?1 AND id = ?2
                 LIMIT 1",
                params![workspace_id.as_str(), receipt_id],
                setup_receipt_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn setup_receipt_for_recipe(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        recipe_hash: &str,
        states: &[&str],
    ) -> Result<Option<SetupReceiptRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, command, state, receipt_json, updated_at,
                    recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
                    output_path, redacted_summary, setup_identity_hash, readiness_state,
                    readiness_reason, readiness_remedy
             FROM setup_receipts
             WHERE workspace_id = ?1
               AND project_id = ?2
               AND recipe_hash = ?3
             ORDER BY updated_at DESC, id DESC",
        )?;
        let mut rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str(), recipe_hash],
            setup_receipt_from_row,
        )?;
        rows.find_map(|row| match row {
            Ok(receipt) if states.contains(&receipt.state.as_str()) => Some(Ok(receipt)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .transpose()
        .map_err(Into::into)
    }

    pub fn setup_receipt_for_command(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        command: &str,
        states: &[&str],
    ) -> Result<Option<SetupReceiptRecord>, MetadataError> {
        let mut statement = self.connection.prepare(
            "SELECT id, workspace_id, project_id, command, state, receipt_json, updated_at,
                    recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
                    output_path, redacted_summary, setup_identity_hash, readiness_state,
                    readiness_reason, readiness_remedy
             FROM setup_receipts
             WHERE workspace_id = ?1
               AND project_id = ?2
               AND command = ?3
             ORDER BY updated_at DESC, id DESC",
        )?;
        let mut rows = statement.query_map(
            params![workspace_id.as_str(), project_id.as_str(), command],
            setup_receipt_from_row,
        )?;
        rows.find_map(|row| match row {
            Ok(receipt) if states.contains(&receipt.state.as_str()) => Some(Ok(receipt)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .transpose()
        .map_err(Into::into)
    }

    pub fn setup_receipt_for_identity(
        &self,
        workspace_id: &WorkspaceId,
        project_id: &ProjectId,
        setup_identity_hash: &str,
    ) -> Result<Option<SetupReceiptRecord>, MetadataError> {
        self.connection
            .query_row(
                "SELECT id, workspace_id, project_id, command, state, receipt_json, updated_at,
                        recipe_hash, approval_state, trigger, cwd, os, arch, env_profile,
                        output_path, redacted_summary, setup_identity_hash, readiness_state,
                        readiness_reason, readiness_remedy
                 FROM setup_receipts
                 WHERE workspace_id = ?1
                   AND project_id = ?2
                   AND setup_identity_hash = ?3
                 ORDER BY updated_at DESC,
                          CASE state
                            WHEN 'failed' THEN 0
                            WHEN 'blocked' THEN 0
                            WHEN 'completed' THEN 1
                            WHEN 'approved' THEN 2
                            WHEN 'approval-required' THEN 2
                            ELSE 3
                          END,
                          id DESC
                 LIMIT 1",
                params![
                    workspace_id.as_str(),
                    project_id.as_str(),
                    setup_identity_hash
                ],
                setup_receipt_from_row,
            )
            .optional()
            .map_err(Into::into)
    }
}
