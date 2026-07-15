use super::*;

pub fn write_source_pack_refs(
    workspace_id: WorkspaceId,
    records: &[PackRecordRef<'_>],
    target_raw_pack_size: usize,
    key: StorageKey,
    key_epoch: u32,
) -> Result<Vec<PackWriteOutput>, PackfileError> {
    let mut packs = Vec::new();
    write_source_pack_ref_batches_with(
        workspace_id,
        records,
        target_raw_pack_size,
        key,
        key_epoch,
        |writer, batch| {
            packs.push(writer.write_refs(batch)?);
            Ok(())
        },
    )?;
    Ok(packs)
}

pub fn write_source_packs(
    workspace_id: WorkspaceId,
    records: &[PackRecordInput],
    target_raw_pack_size: usize,
    key: StorageKey,
    key_epoch: u32,
) -> Result<Vec<PackWriteOutput>, PackfileError> {
    let mut packs = Vec::new();
    write_source_packs_with(
        workspace_id,
        records,
        target_raw_pack_size,
        key,
        key_epoch,
        |pack| {
            packs.push(pack);
            Ok(())
        },
    )?;
    Ok(packs)
}

pub fn write_source_packs_with(
    workspace_id: WorkspaceId,
    records: &[PackRecordInput],
    target_raw_pack_size: usize,
    key: StorageKey,
    key_epoch: u32,
    mut on_pack: impl FnMut(PackWriteOutput) -> Result<(), PackfileError>,
) -> Result<(), PackfileError> {
    write_source_pack_batches_with(
        workspace_id,
        records,
        target_raw_pack_size,
        key,
        key_epoch,
        |writer, batch| on_pack(writer.write(batch)?),
    )
}

pub fn write_source_pack_batches_with(
    workspace_id: WorkspaceId,
    records: &[PackRecordInput],
    target_raw_pack_size: usize,
    key: StorageKey,
    key_epoch: u32,
    mut on_pack: impl FnMut(&PackWriter, &[PackRecordInput]) -> Result<(), PackfileError>,
) -> Result<(), PackfileError> {
    if records.is_empty() {
        return Ok(());
    }

    let target_raw_pack_size = target_raw_pack_size.max(1);
    let batch_seed = new_pack_batch_seed(&workspace_id, records, target_raw_pack_size, key_epoch);
    let mut batch_start = 0_usize;
    let mut batch_raw_size = 0_usize;
    let mut sequence = 1_usize;

    for (index, record) in records.iter().enumerate() {
        if index > batch_start && batch_raw_size + record.bytes.len() > target_raw_pack_size {
            let writer = PackWriter::new(
                workspace_id.clone(),
                opaque_pack_id(&batch_seed, sequence),
                key,
                key_epoch,
            );
            on_pack(&writer, &records[batch_start..index])?;
            sequence += 1;
            batch_start = index;
            batch_raw_size = 0;
        }
        batch_raw_size += record.bytes.len();
    }

    if batch_start < records.len() {
        let writer = PackWriter::new(
            workspace_id,
            opaque_pack_id(&batch_seed, sequence),
            key,
            key_epoch,
        );
        on_pack(&writer, &records[batch_start..])?;
    }

    Ok(())
}
