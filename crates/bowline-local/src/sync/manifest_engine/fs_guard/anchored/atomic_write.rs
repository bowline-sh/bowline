use std::io::{self, Write};
use std::os::fd::AsFd;

use rustix::fs::{FileType, RenameFlags};

use super::*;

impl AnchoredDirectory {
    pub fn write_private_file_atomic(
        &self,
        leaf: &LeafName,
        bytes: &[u8],
    ) -> io::Result<AtomicWrite> {
        self.clean_stale_atomic_write_temps()?;
        let (temp, mut file) = {
            let mut created = None;
            for _ in 0..16 {
                let candidate = LeafName::atomic_write_sibling()?;
                match self.create_private_file(&candidate) {
                    Ok(file) => {
                        created = Some((candidate, file));
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            created.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "atomic write temp name attempts exhausted",
                )
            })?
        };
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            unlink_if_present(self, &temp)?;
            return Err(error);
        }
        drop(file);
        if let Err(error) = self.rename(&temp, leaf) {
            unlink_if_present(self, &temp)?;
            return Err(error);
        }
        Ok(AtomicWrite::Written)
    }

    fn clean_stale_atomic_write_temps(&self) -> io::Result<()> {
        let cutoff = unix_seconds()?.saturating_sub(RECOVERY_CLEANUP_GRACE.as_secs());
        self.clean_atomic_write_temps_before(cutoff)
    }

    pub(super) fn clean_atomic_write_temps_before(&self, cutoff: u64) -> io::Result<()> {
        let mut changed = false;
        for entry in self.entries()? {
            let Some(name) = entry.name.as_str() else {
                continue;
            };
            let Some(nonce) = name
                .strip_prefix(".bowline-materialize-atomic-")
                .and_then(|name| name.strip_suffix(".tmp"))
            else {
                continue;
            };
            if !is_lower_hex_nonce(nonce) {
                continue;
            }
            let Some(stat) = stat_at(self.directory.as_fd(), &entry.name)? else {
                continue;
            };
            let Some(mtime) = u64::try_from(stat.st_mtime).ok() else {
                continue;
            };
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || mtime > cutoff {
                continue;
            }
            let Some(identity) = stat_identity(&stat) else {
                continue;
            };
            let mut quarantined = None;
            for _ in 0..16 {
                let quarantine = LeafName::atomic_write_sibling()?;
                match rustix::fs::renameat_with(
                    &self.directory,
                    entry.name.as_c_str(),
                    &self.directory,
                    quarantine.as_c_str(),
                    RenameFlags::NOREPLACE,
                ) {
                    Ok(()) => {
                        quarantined = Some(quarantine);
                        break;
                    }
                    Err(rustix::io::Errno::EXIST) => continue,
                    Err(rustix::io::Errno::NOENT) => break,
                    Err(error) => return Err(io::Error::from(error)),
                }
            }
            let Some(quarantine) = quarantined else {
                continue;
            };
            if stat_at(self.directory.as_fd(), &quarantine)?
                .as_ref()
                .and_then(stat_identity)
                != Some(identity)
            {
                if let Err(error) = rustix::fs::renameat_with(
                    &self.directory,
                    quarantine.as_c_str(),
                    &self.directory,
                    entry.name.as_c_str(),
                    RenameFlags::NOREPLACE,
                ) {
                    return Err(io::Error::other(format!(
                        "atomic temp replacement preserved in quarantine: {error}"
                    )));
                }
                return Err(io::Error::other("atomic temp inode changed before cleanup"));
            }
            unlink_if_present(self, &quarantine)?;
            changed = true;
        }
        if changed {
            self.sync()?;
        }
        Ok(())
    }
}
