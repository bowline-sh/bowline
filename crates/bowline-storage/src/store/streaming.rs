use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn write_verified_object_to_temp(
    root: &Path,
    key: &ObjectKey,
    metadata: ObjectMetadata,
    object_path: PathBuf,
) -> Result<(PathBuf, u64), ByteStoreError> {
    let mut source =
        fs::File::open(object_path).map_err(|error| map_missing(error, key, "object"))?;
    let temp_path = verified_read_temp_path(root, key);
    let result = (|| {
        let mut temp = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        let mut hasher = blake3::Hasher::new();
        let mut byte_len = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            byte_len = byte_len.checked_add(read as u64).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "object length overflow")
            })?;
            temp.write_all(&buffer[..read])?;
        }
        temp.sync_all()?;
        let hash = format!("b3_{}", hasher.finalize().to_hex());
        if byte_len != metadata.byte_len || hash != metadata.hash {
            return Err(ByteStoreError::CorruptObject {
                key: metadata.key.clone(),
                reason: "object bytes did not match metadata",
            });
        }
        Ok(byte_len)
    })();
    match result {
        Ok(byte_len) => Ok((temp_path, byte_len)),
        Err(error) => {
            match fs::remove_file(&temp_path) {
                Ok(()) => {}
                Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => {}
                Err(cleanup) => return Err(ByteStoreError::Io(cleanup)),
            }
            Err(error)
        }
    }
}

fn verified_read_temp_path(root: &Path, key: &ObjectKey) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_nanos();
    objects_dir(root).join(format!(
        ".{}.read-{}-{}.bowline-tmp",
        key.as_str(),
        std::process::id(),
        nonce
    ))
}
