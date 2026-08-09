# Bowline release durability helper

This internal binary provides one release-ledger v2 persistence authority:

```text
bowline-release-durability sync-inherited-fd --kind file --fd 3
bowline-release-durability sync-inherited-fd --kind directory --fd 3
```

Release-ledger integration opens the target without following a symbolic link,
keeps that exact descriptor live, and passes it into child slot 3:

```js
const result = spawnSync(
  helperPath,
  ["sync-inherited-fd", "--kind", kind, "--fd", "3"],
  { stdio: ["ignore", "pipe", "pipe", descriptor], encoding: "utf8" },
);
```

`helperPath` is an explicit configured or bundled absolute path. Production
integration must not resolve this privileged barrier helper through `PATH`.

The caller keeps the descriptor open through `spawnSync`, validates the single
JSON response and zero status, and closes it only after the helper returns. The
helper accepts only descriptor slot 3, duplicates it into an owned close-on-exec
descriptor, validates its target with `fstat`, and never receives or reopens the
path. It never emits a descriptor, path, or raw operating-system error.

On macOS, both target kinds use `fcntl(F_FULLFSYNC)` and advertise
`darwin_f_fullfsync_durability_v1`. On Linux, both use descriptor `fsync` and
advertise `linux_fsync_durability_v1`. Failure of the requested barrier fails
closed; directory barriers are never downgraded to a weaker operation.

Standard output is one JSON object with only `schemaVersion`, `operation`,
`result`, `failureCode`, and `platformContract`. Exit status is zero only when
the inherited target is durably synchronized.
