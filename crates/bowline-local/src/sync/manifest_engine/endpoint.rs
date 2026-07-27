//! What the engine measures about the volume it is syncing, and the decisions
//! that measurement settles.
//!
//! Two properties of an endpoint filesystem change what "unchanged" and "the
//! same file" mean, and neither may be guessed from the OS name — a macOS
//! workspace can sit on an exFAT stick and a Linux one on a case-insensitive
//! network mount. So both are measured the way Mutagen probes endpoint
//! behaviour: write something with a known shape, read back what survived.
//!
//! **Timestamp granularity** fixes the width of git's racily-clean window (named
//! in 2005): a stat comparison proves a file unchanged only when no write could
//! have landed *after* the engine recorded the file and still produced the
//! timestamp the record holds. Inside that window the only honest answer is to
//! read the bytes; outside it, reading them is pure waste. Getting the window
//! right is what makes a pull echo — a pull installs 10k files, the watcher
//! reports 10k paths, the next push must decide whether any of them really
//! changed — cost zero content opens instead of 10k. Assuming nanoseconds on an
//! HFS+ volume would silently miss a same-size rewrite; assuming seconds
//! everywhere throws the win away on every modern filesystem.
//!
//! **The window is proved, never assumed.** A probe can only report what the
//! volume *stores*, which is not what the volume *stamps*: Linux writes take
//! their mtime from the coarse per-tick clock (1–10 ms), while `utimensat`
//! round-trips full nanoseconds, so the honest answer to "how finely does this
//! volume record time?" is "finer than it can tell two writes apart". Reading a
//! `verified_at` off the process wall clock and comparing it against a
//! volume-stamped mtime therefore compares two clocks that disagree by up to one
//! tick, and every nanosecond of that disagreement is a same-size rewrite the
//! engine declares unchanged. So an ancestor row never *claims* an observation
//! instant. [`prove_rows`] samples the endpoint volume's OWN clock and then
//! re-observes each row, and stamps [`FileRecord::verified_at`] only where the
//! clock has already moved past the file's timestamp — the one state in which no
//! later write can reproduce it. A row the cycle could not prove carries `None`
//! and is read, which is exactly the outcome an unprovable row deserves.
//!
//! The proof runs on **ctime**, not mtime. `utimensat` — `tar -x`, `rsync -t`,
//! `cp -p`, every archive extractor — restores an arbitrary mtime over new
//! bytes, so an mtime older than the window proves nothing at all; ctime is
//! stamped by the kernel on every inode change and no API can move it backwards.
//! Both still take part in the fingerprint comparison, so a restored mtime over
//! changed content is caught by the ctime it could not restore.
//!
//! Soundness does not depend on the probe being right, which matters because it
//! cannot be. Let `q` be however coarsely the volume really stamps times. A write
//! that lands after the cycle sampled the clock at `V` has `q(write) >= V`. A row
//! is only stamped when its recorded timestamp is strictly below `V` at endpoint
//! resolution, so any later write lands in a strictly higher bucket and cannot
//! reproduce the recorded fingerprint. Under-reporting the granularity narrows
//! the buckets (the engine distinguishes more than the volume does, so it reads
//! more); over-reporting widens them (fewer rows clear the strictly-below test,
//! so it reads more). Both directions cost syscalls. Neither loses a write.
//!
//! **Name folding** fixes which spellings of a name denote one file. APFS is
//! normalization-*insensitive* and normalization-*preserving*: a file created as
//! NFD on macOS and as NFC on Linux is the same file on disk but two distinct
//! byte strings. Publishing both spellings makes the engine observe one file
//! twice, push it twice, and let apply ping-pong between the two entries,
//! manufacturing duplicate conflict-asides for a file nobody touched. Case is
//! the same problem one axis over.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use unicode_normalization::{UnicodeNormalization, is_nfc};

use super::fs_guard::{
    MetadataNsecPair, ObserveOutcome, Observed, PRIVATE_FILE_MODE, observe_classified,
};
use super::manifest::WorkspacePath;
use super::store::FileRecord;

/// The engine's private probe files. They live under `.bowline`, which the stat
/// walk skips, so probing never manufactures workspace churn.
const TIMESTAMP_PROBE_LEAF: &str = "timestamp-granularity.probe";
/// Probe file for the endpoint volume's own clock. Distinct from the granularity
/// probe: that one overwrites its mtime with a known value, this one keeps the
/// mtime the volume stamped at create.
const CLOCK_PROBE_LEAF: &str = "endpoint-clock.probe";
/// Decomposed (NFD) spelling of `é`: `e` followed by COMBINING ACUTE ACCENT.
const DECOMPOSED_PROBE_LEAF: &str = "normalization-e\u{0301}.probe";
/// Precomposed (NFC) spelling of the same name. A filesystem that folds the two
/// resolves this to the file created under [`DECOMPOSED_PROBE_LEAF`].
const PRECOMPOSED_PROBE_LEAF: &str = "normalization-\u{e9}.probe";
/// Mixed-case probe pair. Every letter differs in case between the two, so a
/// filesystem that folds only the leading character cannot pass by accident.
const MIXED_CASE_PROBE_LEAF: &str = "CaSe-Aa.probe";
const SWAPPED_CASE_PROBE_LEAF: &str = "cAsE-aA.probe";

/// A timestamp the probe writes and reads back. The seconds are odd and the
/// nanoseconds are non-zero, so a filesystem that truncates to whole seconds and
/// one that rounds to even seconds (FAT) are told apart by what survives.
const PROBE_SECONDS: i64 = 1_000_000_001;
const PROBE_NANOS: i64 = 123_456_789;

/// How finely the endpoint filesystem records modification times.
///
/// Not a bare integer: it is the width of a correctness window, and passing the
/// wrong number here is a silent data-loss bug rather than a type error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimestampGranularity(i64);

impl TimestampGranularity {
    /// Nanosecond timestamps: ext4, xfs, btrfs, APFS, ZFS.
    pub const NANOSECOND: Self = Self(1);
    /// Whole-second timestamps: HFS+, older network filesystems.
    pub const SECOND: Self = Self(1_000_000_000);
    /// Two-second timestamps: FAT-family volumes.
    pub const TWO_SECONDS: Self = Self(2_000_000_000);

    pub fn nanos(self) -> i64 {
        self.0
    }

    /// The start of the bucket `nanos` falls in — the coarsest value of that
    /// timestamp this endpoint could have recorded.
    ///
    /// Euclidean division so a pre-1970 timestamp floors downwards like every
    /// other one; truncating division would round it towards the epoch and put
    /// two timestamps the volume cannot tell apart in different buckets.
    pub fn bucket(self, nanos: i64) -> i64 {
        nanos.div_euclid(self.0).saturating_mul(self.0)
    }

    /// Whether this endpoint could have recorded `left` and `right` as one
    /// timestamp.
    pub fn indistinguishable(self, left: i64, right: i64) -> bool {
        self.bucket(left) == self.bucket(right)
    }
}

/// A reading of the endpoint volume's OWN clock — the clock that stamps the
/// mtimes and ctimes the engine compares.
///
/// Not an `i64` and not a [`SystemTime`](std::time::SystemTime): the whole
/// failure this type exists to prevent is a row verified against the process
/// wall clock, which on any volume that stamps times from a coarse tick runs
/// ahead of the timestamps it is compared with. The only way to obtain one is
/// [`sample_endpoint_clock`], so "an instant this volume had actually reached"
/// is a fact the type carries rather than a comment someone has to honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EndpointInstant(i64);

impl EndpointInstant {
    /// Rehydrate a reading the store persisted. The ONLY constructor besides
    /// [`sample_endpoint_clock`], and it exists because a `verified_at` column
    /// has to round-trip; anything that reaches for it to invent an instant is
    /// reintroducing the wall-clock bug this type exists to make unspellable.
    pub(super) fn from_stored_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    pub fn nanos(self) -> i64 {
        self.0
    }
}

/// Measure the endpoint's timestamp granularity by writing a known time and
/// reading back what the filesystem kept.
///
/// This measures what the volume can STORE, which is an upper bound on what it
/// can distinguish and nothing more: ext4 round-trips the nanoseconds
/// `utimensat` hands it while stamping writes from a clock that only moves once
/// per timer tick. Correctness may therefore never rest on this number — see the
/// module header for why it does not — so it is free to be an estimate. Any
/// failure answers [`TimestampGranularity::TWO_SECONDS`], the coarsest reading
/// and so the most verification.
///
/// Probed once when the workspace is accepted, alongside
/// [`probe_name_folding`]: it is a property of the mounted volume, and every
/// cycle must answer "one timestamp or two?" the same way.
pub fn probe_timestamp_granularity(engine_dir: &Path) -> TimestampGranularity {
    measure_timestamp_granularity(engine_dir).unwrap_or(TimestampGranularity::TWO_SECONDS)
}

fn measure_timestamp_granularity(engine_dir: &Path) -> io::Result<TimestampGranularity> {
    use rustix::fs::{Timespec, Timestamps, UTIME_OMIT};

    let probe = engine_dir.join(TIMESTAMP_PROBE_LEAF);
    create_probe_file(engine_dir, &probe)?;

    let written = Timestamps {
        last_access: Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        last_modification: Timespec {
            tv_sec: PROBE_SECONDS,
            tv_nsec: PROBE_NANOS as _,
        },
    };
    let result = rustix::fs::utimensat(
        rustix::fs::CWD,
        &probe,
        &written,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(io::Error::from)
    .and_then(|()| fs::symlink_metadata(&probe));
    let _ = fs::remove_file(&probe);
    let metadata = result?;

    if metadata.mtime_nsec() == PROBE_NANOS {
        return Ok(TimestampGranularity::NANOSECOND);
    }
    if metadata.mtime() == PROBE_SECONDS {
        return Ok(TimestampGranularity::SECOND);
    }
    Ok(TimestampGranularity::TWO_SECONDS)
}

/// Read the endpoint volume's own clock, by creating a file on it and asking
/// what time the volume stamped.
///
/// `None` — no row provable this cycle — whenever the reading cannot be shown to
/// describe the volume the workspace lives on. That includes the probe failing
/// outright and, deliberately, the engine state directory sitting on a DIFFERENT
/// device from the workspace root: a work view keeps its engine state under the
/// daemon's state root, and a clock read from that volume says nothing about the
/// volume whose mtimes the rows carry. `st_dev` is the proof, so the answer is
/// about the measured volume rather than about a path convention.
pub fn sample_endpoint_clock(engine_dir: &Path, workspace_root: &Path) -> Option<EndpointInstant> {
    read_endpoint_clock(engine_dir, workspace_root).ok()
}

fn read_endpoint_clock(engine_dir: &Path, workspace_root: &Path) -> io::Result<EndpointInstant> {
    let probe = engine_dir.join(CLOCK_PROBE_LEAF);
    create_probe_file(engine_dir, &probe)?;
    let result = fs::symlink_metadata(&probe);
    let _ = fs::remove_file(&probe);
    let metadata = result?;
    let root = fs::metadata(workspace_root)?;
    if metadata.dev() != root.dev() {
        return Err(io::Error::new(
            io::ErrorKind::CrossesDevices,
            "engine state directory is not on the workspace volume",
        ));
    }
    Ok(EndpointInstant(metadata.mtime_nsec_pair()))
}

// ---- name folding ----------------------------------------------------------

/// Whether the endpoint filesystem can tell two Unicode normalization forms of
/// one name apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizationForm {
    /// The NFC and NFD spellings of a name resolve to the SAME file (APFS,
    /// HFS+). The volume is normalization-*preserving* — it hands back whatever
    /// spelling created the file — so the engine must pick one spelling for
    /// itself, and is free to: either one opens the file.
    Insensitive,
    /// Every distinct byte sequence is a distinct name (ext4, xfs, btrfs). The
    /// bytes on disk ARE the name; rewriting them would name a file that does
    /// not exist.
    Sensitive,
}

impl NormalizationForm {
    /// The verdict for a probe that created a decomposed name and asked whether
    /// the precomposed one resolves.
    fn from_folds(folds: bool) -> Self {
        if folds {
            Self::Insensitive
        } else {
            Self::Sensitive
        }
    }
}

/// Whether the endpoint filesystem can tell two cases of one name apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseForm {
    /// `README.md` and `readme.md` are one file (APFS default, NTFS, exFAT).
    Insensitive,
    /// They are two files (ext4, xfs, btrfs, case-sensitive APFS volumes).
    Sensitive,
}

impl CaseForm {
    /// The verdict for a probe that created a mixed-case name and asked whether
    /// the case-swapped one resolves.
    fn from_folds(folds: bool) -> Self {
        if folds {
            Self::Insensitive
        } else {
            Self::Sensitive
        }
    }
}

/// Which spellings of a name the endpoint filesystem cannot tell apart.
///
/// A probed verdict rather than a per-OS constant: the property belongs to the
/// volume, and a macOS workspace on an exFAT stick or a Linux workspace on a
/// case-insensitive network mount both break the OS-name shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameFolding {
    normalization: NormalizationForm,
    case: CaseForm,
}

impl NameFolding {
    /// The verdict for a filesystem that distinguishes every byte sequence.
    ///
    /// Also the answer when the probe cannot run, and that direction is the safe
    /// one: calling a case-sensitive volume insensitive would merge two
    /// genuinely different files into one manifest entry and lose one of them,
    /// while calling an insensitive volume sensitive costs at worst a redundant
    /// conflict-aside, which preserves every byte by construction.
    pub const EXACT: Self = Self {
        normalization: NormalizationForm::Sensitive,
        case: CaseForm::Sensitive,
    };

    pub fn new(normalization: NormalizationForm, case: CaseForm) -> Self {
        Self {
            normalization,
            case,
        }
    }

    pub fn normalization(self) -> NormalizationForm {
        self.normalization
    }

    pub fn case(self) -> CaseForm {
        self.case
    }

    /// The spelling a path is published under.
    ///
    /// NFC is the wire form — Dropbox's and Google Drive's choice, for the
    /// reason a manifest shares: a name is a user-visible identifier, and two
    /// byte strings a human reads as one name must not become two entries. The
    /// rewrite is applied only where the probe proves it lossless; on a
    /// normalization-sensitive volume the observed bytes are the only name that
    /// opens the file, so they are published verbatim.
    ///
    /// Case is NEVER rewritten. A case-insensitive filesystem is still
    /// case-*preserving*, so `README.md` is the user's chosen spelling and
    /// folding it would rename the file on every peer.
    pub fn canonical_spelling(self, path: &WorkspacePath) -> WorkspacePath {
        match self.normalization {
            NormalizationForm::Sensitive => path.clone(),
            NormalizationForm::Insensitive => nfc_path(path),
        }
    }

    /// The key under which this endpoint cannot tell two names apart.
    ///
    /// Names the equivalence class rather than a file, so unlike
    /// [`Self::canonical_spelling`] it may fold case. Manifest decode uses it to
    /// find remote paths that would collide when materialized here.
    pub fn fold(self, path: &str) -> String {
        let normalized = match self.normalization {
            NormalizationForm::Sensitive => path.to_string(),
            NormalizationForm::Insensitive if is_nfc(path) => path.to_string(),
            NormalizationForm::Insensitive => path.nfc().collect(),
        };
        match self.case {
            CaseForm::Sensitive => normalized,
            CaseForm::Insensitive => normalized.to_lowercase(),
        }
    }
}

/// The precomposed (NFC) spelling of a workspace path.
///
/// Allocation-free when the path is already NFC, which every ASCII path and
/// every path a Bowline device produced is — the copy is paid only for a name
/// some other tool decomposed.
pub fn nfc_path(path: &WorkspacePath) -> WorkspacePath {
    if is_nfc(path.as_str()) {
        return path.clone();
    }
    WorkspacePath::new(path.as_str().nfc().collect::<String>())
}

/// Measure which spellings the endpoint filesystem folds together, by creating a
/// file under one spelling and asking for it under the other.
///
/// Any failure answers [`NameFolding::EXACT`]; see that constant for why it is
/// the safe direction.
pub fn probe_name_folding(engine_dir: &Path) -> NameFolding {
    NameFolding {
        normalization: NormalizationForm::from_folds(
            probe_resolves_as(engine_dir, DECOMPOSED_PROBE_LEAF, PRECOMPOSED_PROBE_LEAF)
                .unwrap_or(false),
        ),
        case: CaseForm::from_folds(
            probe_resolves_as(engine_dir, MIXED_CASE_PROBE_LEAF, SWAPPED_CASE_PROBE_LEAF)
                .unwrap_or(false),
        ),
    }
}

/// Create a probe file named `created` and report whether the filesystem also
/// resolves it under `alternate`. Both spellings are unlinked before returning,
/// whatever the answer.
fn probe_resolves_as(engine_dir: &Path, created: &str, alternate: &str) -> io::Result<bool> {
    let probe = engine_dir.join(created);
    let other = engine_dir.join(alternate);
    // A stale `alternate` left by an interrupted probe would answer "folded" on
    // every filesystem. Clear both names before creating anything.
    let _ = fs::remove_file(&probe);
    let _ = fs::remove_file(&other);
    create_probe_file(engine_dir, &probe)?;
    let resolved = fs::symlink_metadata(&other).is_ok();
    let _ = fs::remove_file(&probe);
    let _ = fs::remove_file(&other);
    Ok(resolved)
}

fn create_probe_file(engine_dir: &Path, probe: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    // Never bring the workspace root into existence. Telling an unmounted volume
    // from an empty workspace is the root sentinel's entire job, and a probe that
    // `mkdir -p`'d its way to a probe file would hand it a directory that looks
    // like an empty workspace. A probe that cannot run answers with the safe
    // default instead.
    if !engine_dir.parent().is_some_and(|parent| parent.is_dir()) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "workspace root is not present",
        ));
    }
    fs::create_dir_all(engine_dir)?;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(PRIVATE_FILE_MODE)
        .open(probe)?;
    Ok(())
}

// ---- stat trust ------------------------------------------------------------

/// Whether a matching stat fingerprint is allowed to settle a file as unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatTrust {
    /// A watcher has been attached continuously since the ancestor row was
    /// written, so the only undetectable write is one inside the racy window.
    OutsideRacyWindow(TimestampGranularity),
    /// Nothing bounds how long the tree went unobserved — a restart seed, a
    /// watcher-overflow recovery, a work-view capture that owns no watcher — so
    /// a same-size rewrite at any point in that gap could carry a stat the
    /// engine already recorded. Read the bytes.
    Never,
}

impl StatTrust {
    /// Whether `observed` may be declared unchanged against `record` without
    /// reading the file.
    ///
    /// The caller has already established kind, size, and mode; this decides the
    /// stat identity and whether that identity is *provable*.
    ///
    /// Two conditions, and both are about the same instant. The fingerprints must
    /// match as this endpoint records them — inode and device exactly, timestamps
    /// at endpoint resolution, because two timestamps inside one bucket are one
    /// timestamp as far as this volume is concerned and pretending otherwise
    /// makes the comparison finer than the evidence. And the observed ctime must
    /// sit strictly below the bucket of `verified_at`, the endpoint-clock reading
    /// [`prove_rows`] proved the row against: a write landing after that reading
    /// carries a ctime at or above it, so a ctime strictly below could not have
    /// come from one.
    ///
    /// ctime rather than mtime because mtime is writable. `tar -x`, `rsync -t`
    /// and `cp -p` all restore an arbitrary mtime over brand-new bytes, so an old
    /// mtime is not evidence of an old write; ctime moves on every inode change
    /// and no interface can move it back.
    pub fn settles(self, record: &FileRecord, observed: &Observed) -> bool {
        let Self::OutsideRacyWindow(granularity) = self else {
            return false;
        };
        let Some(verified_at) = record.verified_at else {
            // The cycle that wrote this row could not prove it, so there is no
            // instant to compare against and nothing to conclude.
            return false;
        };
        same_stat(granularity, record, observed)
            && granularity.bucket(observed.fingerprint.ctime_ns)
                < granularity.bucket(verified_at.nanos())
    }
}

/// Whether the ancestor row and the observation are the same file state as far
/// as this endpoint can record it.
fn same_stat(granularity: TimestampGranularity, record: &FileRecord, observed: &Observed) -> bool {
    let recorded = &record.fingerprint;
    let seen = &observed.fingerprint;
    recorded.inode == seen.inode
        && recorded.dev == seen.dev
        && granularity.indistinguishable(recorded.mtime_ns, seen.mtime_ns)
        && granularity.indistinguishable(recorded.ctime_ns, seen.ctime_ns)
}

/// Stamp each adopted row with the endpoint instant that PROVES it, by reading
/// the volume's clock and then re-observing every row against that reading.
///
/// This is the only place `verified_at` is ever set. The order is the whole
/// mechanism and it is enforced here rather than documented: the clock is
/// sampled FIRST, so every observation below happened at or after the instant
/// the rows will claim, and a row is stamped only when its ctime is strictly
/// below that instant's bucket. A row the cycle cannot prove — the file changed
/// under the cycle, vanished, or was written in the very tick the clock is still
/// sitting in — keeps `None` and is read on the next push rather than trusted.
///
/// Cost is one clock sample plus one stat per adopted row: proportional to the
/// change (invariant C2), and it is what buys the pull echo its zero content
/// opens, since a row proved here settles from a stat forever after.
pub(super) fn prove_rows<'a>(
    workspace_root: &Path,
    engine_dir: &Path,
    granularity: TimestampGranularity,
    rows: impl Iterator<Item = (&'a WorkspacePath, &'a mut FileRecord)>,
) {
    let Some(sampled) = sample_endpoint_clock(engine_dir, workspace_root) else {
        // No reading of this volume's clock, so nothing is provable this cycle.
        // Every row keeps the `None` it was built with.
        return;
    };
    for (path, record) in rows {
        record.verified_at = prove_row(workspace_root, granularity, sampled, path, record);
    }
}

fn prove_row(
    workspace_root: &Path,
    granularity: TimestampGranularity,
    sampled: EndpointInstant,
    path: &WorkspacePath,
    record: &FileRecord,
) -> Option<EndpointInstant> {
    if granularity.bucket(record.fingerprint.ctime_ns) >= granularity.bucket(sampled.nanos()) {
        return None;
    }
    let ObserveOutcome::Present(observed) = observe_classified(workspace_root, path) else {
        return None;
    };
    if observed.kind != record.kind || !same_stat(granularity, record, &observed) {
        return None;
    }
    Some(sampled)
}

#[cfg(test)]
#[path = "endpoint/tests.rs"]
mod tests;
