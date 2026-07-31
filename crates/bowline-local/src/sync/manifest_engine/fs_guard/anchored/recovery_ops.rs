use std::fs;
use std::io::{self, Seek, SeekFrom};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;

use rustix::fs::{FileType, RenameFlags};
use rustix::io::Errno;

use super::*;

impl AnchoredDirectory {
    pub(super) fn copy_staged_file_atomic(
        &self,
        owner_directory: &Self,
        source: &mut fs::File,
        destination_leaf: &LeafName,
        final_mode: FileMode,
    ) -> io::Result<GuardedWrite> {
        self.clean_owned_recovery_temps(owner_directory)?;
        let destination_directory_identity = directory_identity(&self.directory)?;
        let owner_created_at = unix_seconds()?;
        let (temp, owner, mut destination) = {
            let mut created = None;
            for _ in 0..16 {
                let candidate = LeafName::recovery_sibling()?;
                let owner = candidate
                    .recovery_owner_sibling()
                    .ok_or_else(|| io::Error::other("generated recovery owner name invalid"))?;
                let mut owner_file = match owner_directory.create_private_file(&owner) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                };
                if let Err(error) = write_recovery_owner(
                    &mut owner_file,
                    owner_created_at,
                    destination_directory_identity,
                    None,
                    destination_leaf,
                ) {
                    cleanup_recovery_setup(owner_directory, &owner, self, None)?;
                    return Err(error);
                }
                if let Err(error) = owner_directory.sync() {
                    cleanup_recovery_setup(owner_directory, &owner, self, None)?;
                    return Err(error);
                }
                match self.create_private_file(&candidate) {
                    Ok(file) => {
                        let temp_identity = match file.metadata() {
                            Ok(metadata) => file_identity(&metadata),
                            Err(error) => {
                                cleanup_recovery_setup(
                                    owner_directory,
                                    &owner,
                                    self,
                                    Some(&candidate),
                                )?;
                                return Err(error);
                            }
                        };
                        if let Err(error) = publish_completed_recovery_owner(
                            owner_directory,
                            &owner,
                            owner_created_at,
                            destination_directory_identity,
                            temp_identity,
                            destination_leaf,
                        ) {
                            cleanup_recovery_setup(
                                owner_directory,
                                &owner,
                                self,
                                Some(&candidate),
                            )?;
                            return Err(error);
                        }
                        if let Err(error) = owner_directory.sync().and_then(|()| self.sync()) {
                            cleanup_recovery_setup(
                                owner_directory,
                                &owner,
                                self,
                                Some(&candidate),
                            )?;
                            return Err(error);
                        }
                        created = Some((candidate, owner, file));
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        cleanup_recovery_setup(owner_directory, &owner, self, None)?;
                    }
                    Err(error) => {
                        cleanup_recovery_setup(owner_directory, &owner, self, None)?;
                        return Err(error);
                    }
                }
            }
            created.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "recovery temp name attempts exhausted",
                )
            })?
        };
        let copied = (|| {
            source.seek(SeekFrom::Start(0))?;
            io::copy(source, &mut destination)?;
            destination.sync_all()?;
            destination.set_permissions(fs::Permissions::from_mode(final_mode.get()))?;
            destination.sync_all()
        })();
        if let Err(error) = copied {
            cleanup_recovery_setup(owner_directory, &owner, self, Some(&temp))?;
            return Err(error);
        }
        let installed = self.install_copied_temp(&destination, &temp, destination_leaf);
        if installed.is_ok() {
            self.sync()?;
            unlink_if_present(owner_directory, &owner)?;
            if let Some(completion) = owner.recovery_owner_completion_sibling() {
                unlink_if_present(owner_directory, &completion)?;
            }
            owner_directory.sync()?;
        }
        installed
    }

    pub(super) fn clean_owned_recovery_temps(&self, owner_directory: &Self) -> io::Result<()> {
        let destination_identity = directory_identity(&self.directory)?;
        for entry in owner_directory.entries()? {
            let Some(temp) = entry.name.recovery_temp_for_owner() else {
                continue;
            };
            let Some(record) = read_recovery_owner(owner_directory, &entry.name)? else {
                continue;
            };
            if record.directory != destination_identity
                || unix_seconds()?.saturating_sub(record.created_at)
                    < RECOVERY_CLEANUP_GRACE.as_secs()
            {
                continue;
            }
            let recorded_temp = match record.temp {
                Some(recorded_temp) => recorded_temp,
                None if entry.name.is_recovery_owner_completion() => continue,
                None => {
                    let Some(stat) = stat_at(self.directory.as_fd(), &temp)? else {
                        remove_recovery_owner_family(owner_directory, &entry.name)?;
                        continue;
                    };
                    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
                        || stat.st_size != 0
                        || permission_bits(&stat) != PRIVATE_FILE_MODE
                    {
                        continue;
                    }
                    let Some(temp_identity) = stat_identity(&stat) else {
                        continue;
                    };
                    publish_completed_recovery_owner(
                        owner_directory,
                        &entry.name,
                        record.created_at,
                        record.directory,
                        temp_identity,
                        &record.destination_leaf,
                    )?;
                    temp_identity
                }
            };
            let installed_identity = stat_at(self.directory.as_fd(), &record.destination_leaf)?
                .as_ref()
                .and_then(stat_identity);
            if installed_identity == Some(recorded_temp) {
                if let Some(displaced) = stat_at(self.directory.as_fd(), &temp)?
                    .as_ref()
                    .and_then(stat_identity)
                {
                    self.remove_recorded_recovery_temp(&temp, displaced)?;
                }
            } else {
                self.remove_recorded_recovery_temp(&temp, recorded_temp)?;
            }
            remove_recovery_owner_family(owner_directory, &entry.name)?;
        }
        Ok(())
    }

    fn remove_recorded_recovery_temp(
        &self,
        temp: &LeafName,
        recorded: FileIdentity,
    ) -> io::Result<()> {
        let quarantine = temp
            .recovery_quarantine_sibling()
            .ok_or_else(|| io::Error::other("recorded recovery temp name invalid"))?;
        let renamed_now = match rustix::fs::renameat_with(
            &self.directory,
            temp.as_c_str(),
            &self.directory,
            quarantine.as_c_str(),
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => true,
            Err(Errno::NOENT) => false,
            Err(Errno::EXIST) => {
                if stat_at(self.directory.as_fd(), temp)?.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "recovery quarantine name occupied",
                    ));
                }
                false
            }
            Err(error) => return Err(io::Error::from(error)),
        };
        let quarantined = stat_at(self.directory.as_fd(), &quarantine)?
            .as_ref()
            .and_then(stat_identity);
        if quarantined.is_none() {
            return Ok(());
        }
        if quarantined != Some(recorded) {
            if renamed_now {
                rustix::fs::renameat_with(
                    &self.directory,
                    quarantine.as_c_str(),
                    &self.directory,
                    temp.as_c_str(),
                    RenameFlags::NOREPLACE,
                )
                .map_err(io::Error::from)?;
            }
            return Err(io::Error::other(
                "recovery temp inode changed before cleanup",
            ));
        }
        unlink_if_present(self, &quarantine)?;
        self.sync()
    }

    pub(super) fn install_copied_temp(
        &self,
        destination: &fs::File,
        temp: &LeafName,
        destination_leaf: &LeafName,
    ) -> io::Result<GuardedWrite> {
        let opened = destination.metadata()?;
        let exchanged = match self.rename_refusing_symlink(self, temp, destination_leaf) {
            Ok(Some(exchanged)) => exchanged,
            Ok(None) => {
                unlink_if_present(self, temp)?;
                self.sync()?;
                return Ok(GuardedWrite::Blocked);
            }
            Err(error) => {
                let _ = self.unlink(temp);
                return Err(error);
            }
        };
        let named = match stat_at(self.directory.as_fd(), destination_leaf) {
            Ok(named) => named,
            Err(error) => {
                self.rollback_staged_install(self, temp, destination_leaf, exchanged)?;
                let _ = self.unlink(temp);
                return Err(error);
            }
        };
        if named.and_then(|named| stat_identity(&named)) != Some(file_identity(&opened)) {
            self.rollback_staged_install(self, temp, destination_leaf, exchanged)?;
            unlink_if_present(self, temp)?;
            self.sync()?;
            return Ok(GuardedWrite::Blocked);
        }
        unlink_if_present(self, temp)?;
        self.sync()?;
        Ok(GuardedWrite::Written(fingerprint_of(&opened)))
    }
}
