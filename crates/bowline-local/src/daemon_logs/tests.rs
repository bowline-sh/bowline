use std::os::fd::OwnedFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::*;

static NEXT_ROOT: AtomicU32 = AtomicU32::new(0);

struct TempStateRoot(PathBuf);

impl TempStateRoot {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "bowline-daemon-logs-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp state root is creatable");
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempStateRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A stand-in for the descriptor launchd hands the daemon: one this test owns,
/// opened append-mode onto the live log file, which rotation re-points exactly
/// as it re-points the daemon's own stdout.
///
/// The mutex is what a real daemon gets for free from the kernel — it lets a
/// writer keep appending through this descriptor while rotation re-points it,
/// which is the concurrency the loss window used to live in.
struct TestStream {
    descriptor: Mutex<OwnedFd>,
}

impl TestStream {
    fn open(path: &Path) -> Self {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open the live log");
        Self {
            descriptor: Mutex::new(OwnedFd::from(file)),
        }
    }

    fn append(&self, record: &str) {
        let descriptor = self.descriptor.lock().expect("the stream descriptor");
        let written = rustix::io::write(&*descriptor, record.as_bytes()).expect("append a record");
        assert_eq!(written, record.len(), "a short write would tear the record");
    }
}

impl LogStream for TestStream {
    fn current_file(&self) -> io::Result<FileIdentity> {
        descriptor_identity(
            self.descriptor
                .lock()
                .expect("the stream descriptor")
                .as_fd(),
        )
    }

    fn reattach(&self, replacement: BorrowedFd<'_>) -> io::Result<()> {
        let mut descriptor = self.descriptor.lock().expect("the stream descriptor");
        rustix::io::dup2(replacement, &mut descriptor).map_err(io::Error::from)
    }
}

/// A stream that writes nowhere in particular, for the paths where the process
/// is not the writer of the file being capped.
struct ForeignStream(OwnedFd);

impl ForeignStream {
    fn new(path: &Path) -> Self {
        Self(OwnedFd::from(
            fs::File::create(path).expect("open the foreign file"),
        ))
    }
}

impl LogStream for ForeignStream {
    fn current_file(&self) -> io::Result<FileIdentity> {
        descriptor_identity(self.0.as_fd())
    }

    fn reattach(&self, _replacement: BorrowedFd<'_>) -> io::Result<()> {
        panic!("a stream that does not write the file must never be re-pointed")
    }
}

fn write_bytes(path: &Path, len: usize) {
    fs::write(path, vec![b'x'; len]).expect("log file is writable");
}

fn inode_of(path: &Path) -> u64 {
    fs::metadata(path).expect("the file exists").ino()
}

#[test]
fn a_log_under_the_cap_is_left_alone() {
    let root = TempStateRoot::new("under-cap");
    let path = stdout_log_path(root.path());
    write_bytes(&path, 16);
    let stream = TestStream::open(&path);
    let policy = LogRotationPolicy {
        max_bytes: 64,
        retained_generations: 1,
    };

    assert_eq!(
        enforce_log_cap(&path, policy, &stream).expect("enforcement succeeds"),
        LogRotationOutcome::Unchanged
    );
    assert_eq!(fs::metadata(&path).expect("log exists").len(), 16);
    assert!(!generation_path(&path, 1).exists());
}

#[test]
fn a_log_over_the_cap_is_retained_and_the_stream_writes_a_fresh_live_file() {
    let root = TempStateRoot::new("over-cap");
    let path = stdout_log_path(root.path());
    write_bytes(&path, 200);
    let stream = TestStream::open(&path);
    let live_inode_before = inode_of(&path);
    let policy = LogRotationPolicy {
        max_bytes: 64,
        retained_generations: 1,
    };

    assert_eq!(
        enforce_log_cap(&path, policy, &stream).expect("enforcement succeeds"),
        LogRotationOutcome::Rotated
    );

    // The retained generation IS the pre-rotation inode, carried over by a
    // rename. A copy would leave a new inode here and a window in which records
    // reach neither file.
    assert_eq!(inode_of(&generation_path(&path, 1)), live_inode_before);
    assert_eq!(
        fs::metadata(generation_path(&path, 1))
            .expect("one generation was retained")
            .len(),
        200
    );
    // The live path must survive rotation, and the stream must be the thing
    // writing it — otherwise the daemon appends forever into the rotated copy.
    assert_eq!(fs::metadata(&path).expect("log still exists").len(), 0);
    assert_ne!(inode_of(&path), live_inode_before);
    stream.append("after-rotation\n");
    assert_eq!(
        fs::read_to_string(&path).expect("read the live log"),
        "after-rotation\n"
    );
}

/// Every record written while a rotation runs has to survive somewhere. The
/// copy-then-truncate rotation this replaced destroyed exactly the ones written
/// between the copy reaching EOF and the truncation landing.
#[test]
fn records_written_while_a_rotation_runs_are_never_lost() {
    let root = TempStateRoot::new("rotation-loss");
    let path = stdout_log_path(root.path());
    // A live file big enough that any snapshot-then-truncate rotation spends
    // real time on the snapshot: that duration is the window records fall into.
    // It ends on a newline so the first appended record is its own line.
    let mut padding = vec![b'x'; 4 * 1024 * 1024];
    padding[4 * 1024 * 1024 - 1] = b'\n';
    fs::write(&path, padding).expect("log file is writable");
    let stream = TestStream::open(&path);
    let policy = LogRotationPolicy {
        max_bytes: 64 * 1024,
        retained_generations: 1,
    };

    let written = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let record = written.load(Ordering::Relaxed);
                stream.append(&format!("record-{record}\n"));
                written.store(record + 1, Ordering::Relaxed);
            }
        });

        // Let the writer get going, so records straddle the rotation instead of
        // all landing after it.
        while written.load(Ordering::Relaxed) < 1_000 {
            std::hint::spin_loop();
        }
        assert_eq!(
            enforce_log_cap(&path, policy, &stream).expect("enforcement succeeds"),
            LogRotationOutcome::Rotated
        );
        while written.load(Ordering::Relaxed) < 2_000 {
            std::hint::spin_loop();
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().expect("writer thread");
    });

    let total = written.load(Ordering::Relaxed);
    let retained = fs::read_to_string(generation_path(&path, 1)).expect("retained generation");
    let live = fs::read_to_string(&path).expect("live log");
    let survivors: std::collections::HashSet<&str> = retained.lines().chain(live.lines()).collect();
    let lost: Vec<u64> = (0..total)
        .filter(|record| !survivors.contains(format!("record-{record}").as_str()))
        .collect();

    assert!(
        lost.is_empty(),
        "{} of {total} records written across the rotation were destroyed by it: first {:?}",
        lost.len(),
        &lost[..lost.len().min(5)],
    );
}

#[test]
fn retained_generations_are_bounded_by_the_policy() {
    let root = TempStateRoot::new("bounded-generations");
    let path = stderr_log_path(root.path());
    let stream = TestStream::open(&path);
    let policy = LogRotationPolicy {
        max_bytes: 8,
        retained_generations: 2,
    };

    for round in 1..=4_usize {
        write_bytes(&path, 16 + round);
        assert_eq!(
            enforce_log_cap(&path, policy, &stream).expect("enforcement succeeds"),
            LogRotationOutcome::Rotated
        );
    }

    // Newest retained generation holds the last rotated round, the oldest holds
    // the one before it, and nothing accumulates past the policy.
    assert_eq!(
        fs::metadata(generation_path(&path, 1))
            .expect("generation 1 exists")
            .len(),
        20
    );
    assert_eq!(
        fs::metadata(generation_path(&path, 2))
            .expect("generation 2 exists")
            .len(),
        19
    );
    assert!(!generation_path(&path, 3).exists());
    let total: u64 = daemon_log_generations(&path)
        .map(|path| {
            fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        })
        .sum();
    assert!(total <= policy.max_bytes * u64::from(policy.retained_generations + 1) + 32);
}

#[test]
fn a_policy_that_retains_nothing_still_cuts_the_live_file_over_atomically() {
    let root = TempStateRoot::new("no-generations");
    let path = stdout_log_path(root.path());
    write_bytes(&path, 200);
    let stream = TestStream::open(&path);
    let policy = LogRotationPolicy {
        max_bytes: 64,
        retained_generations: 0,
    };

    assert_eq!(
        enforce_log_cap(&path, policy, &stream).expect("enforcement succeeds"),
        LogRotationOutcome::Rotated
    );

    assert_eq!(fs::metadata(&path).expect("log still exists").len(), 0);
    assert!(!generation_path(&path, 1).exists());
    stream.append("after-rotation\n");
    assert_eq!(
        fs::read_to_string(&path).expect("read the live log"),
        "after-rotation\n"
    );
}

/// A daemon started by hand has a terminal on stdout while a stale supervised
/// log sits in the state root. Rotating it would re-point the terminal into the
/// log file and leave the live path with nothing writing it.
#[test]
fn a_log_this_process_does_not_write_is_never_rotated() {
    let root = TempStateRoot::new("foreign-writer");
    let path = stdout_log_path(root.path());
    write_bytes(&path, 200);
    let stream = ForeignStream::new(&root.path().join("somewhere-else"));
    let policy = LogRotationPolicy {
        max_bytes: 64,
        retained_generations: 1,
    };

    assert_eq!(
        enforce_log_cap(&path, policy, &stream).expect("enforcement succeeds"),
        LogRotationOutcome::Unattached
    );
    assert_eq!(fs::metadata(&path).expect("log exists").len(), 200);
    assert!(!generation_path(&path, 1).exists());
}

#[test]
fn a_missing_log_is_not_an_error() {
    let root = TempStateRoot::new("missing");

    enforce_daemon_log_caps(root.path(), LogRotationPolicy::DEFAULT)
        .expect("a state root with no logs enforces cleanly");
}

#[test]
fn both_daemon_streams_are_capped() {
    let root = TempStateRoot::new("both-streams");
    let policy = LogRotationPolicy {
        max_bytes: 8,
        retained_generations: 1,
    };
    for path in daemon_log_paths(root.path()) {
        write_bytes(&path, 64);
    }
    let streams: Vec<(PathBuf, TestStream)> = daemon_log_paths(root.path())
        .into_iter()
        .map(|path| {
            let stream = TestStream::open(&path);
            (path, stream)
        })
        .collect();

    enforce_stream_caps(
        streams
            .iter()
            .map(|(path, stream)| (path.clone(), stream as &dyn LogStream)),
        policy,
    )
    .expect("enforcement succeeds");

    for (path, stream) in &streams {
        assert_eq!(fs::metadata(path).expect("log exists").len(), 0);
        stream.append("live\n");
        assert_eq!(
            fs::read_to_string(path).expect("read the live log"),
            "live\n"
        );
    }
}

/// The plist names these two paths and the daemon re-points these two streams;
/// a table that paired them the other way round would send stderr's records into
/// the file the reader watches for stdout.
#[test]
fn each_log_path_is_paired_with_the_stream_that_writes_it() {
    let root = TempStateRoot::new("pairing");

    assert_eq!(
        daemon_log_streams(root.path()),
        [
            (stdout_log_path(root.path()), StandardStream::Out),
            (stderr_log_path(root.path()), StandardStream::Error),
        ]
    );
}

fn daemon_log_generations(path: &Path) -> impl Iterator<Item = PathBuf> + use<> {
    let path = path.to_path_buf();
    (0..=2_u8).map(move |generation| match generation {
        0 => path.clone(),
        other => generation_path(&path, other),
    })
}
