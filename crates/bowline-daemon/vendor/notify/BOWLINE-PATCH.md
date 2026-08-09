# Bowline notify 8.2.0 patch

This directory is the complete source of `notify` 8.2.0 as published on
crates.io, whose registry checksum is
`4d3d07927151ff8575b7087f245456e549fea62edf0ec4e565a5ee50c8402bc3`.
The upstream project is <https://github.com/notify-rs/notify> and remains
licensed under `LICENSE-CC0`.

Bowline carries a narrow native-coverage patch in `src/fsevent.rs` and
`src/inotify.rs`:

- Darwin exposes typed FSEvents cursors, lifecycle/flag observations, and
  explicit cursor-based stream construction. Coverage streams observe the
  native `HistoryDone` control record before the existing event translator
  suppresses it. A separate worker-exit receipt remains observable even if the
  lifecycle callback cannot run, and bounded run-loop turns close the native
  stop-before-first-run race without affecting coverage authority.
- Linux exposes typed same-event-loop watch-ready and recursive re-walk/drain
  acknowledgements on a control callback distinct from event delivery. Its
  event drain is batch-bounded and cooperatively interruptible so shutdown can
  regain the worker under continuous producers; only an actual `WouldBlock`
  result authorizes a coverage acknowledgement.
- Both native backends expose an explicit shutdown that joins their worker.
- Bowline's focused inotify and FSEvents patch tests live in
  `src/inotify/tests.rs` and
  `src/fsevent/bowline_coverage_tests.rs`, so production source and test source
  retain Bowline's separate file-length caps. The two upstream inline FSEvents
  tests are moved without behavioral changes to
  `src/fsevent/upstream_tests.rs` for the same reason.

The ordinary `FsEventWatcher::new` flags and event translation remain
upstream-compatible. Only `new_with_coverage` opts into `WatchRoot`, an explicit
journal cursor, and the independent coverage-control callback. Both constructors
use the patched owned-worker shutdown path described below.

Apple defines `HistoryDone` as the sentinel after every historical event for an
explicit `sinceWhen` cursor. Dropped-event flags require a covering full scan;
`RootChanged` carries event identifier zero and therefore selects a fresh-stream
handoff rather than cursor replay. The adapter never treats elapsed quiet time
as evidence of any of those facts.

Upstream 8.2.0 purged the shared volume journal when one watcher stopped. Apple
documents `FSEventsPurgeEventsForDeviceUpToEventId` as an exceptional operation
that generally must never be used because it destroys history shared by other
clients. This patch removes that call: shutdown stops, invalidates, releases,
and joins only the stream Bowline owns.

All other source files are the unmodified upstream 8.2.0 release.
The normalized manifest adds only an empty workspace root so this patch can run
its own focused backend tests without joining Bowline's workspace. Its adjacent
lockfile pins those test-only dependency resolutions; Bowline production builds
continue to resolve the patched library through the repository root lockfile.
