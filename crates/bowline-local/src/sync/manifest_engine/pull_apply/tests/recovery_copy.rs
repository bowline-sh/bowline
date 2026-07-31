use std::os::unix::fs::symlink;

use super::*;
use crate::sync::manifest_engine::fs_guard::GuardedWrite;
use crate::sync::manifest_engine::pull_apply::materialize::copy_replacement;

#[test]
fn refuses_a_symlink_leaf_without_touching_its_external_target() {
    let engine = TestEngine::new("recovery-copy-symlink-leaf");
    let external = external_dir("recovery-copy-symlink-leaf-ext");
    let target = external.root().join("secret");
    std::fs::write(&target, b"external bytes").expect("seed external target");
    let source = engine.root().join(".recovery-source");
    std::fs::write(&source, b"recovered bytes").expect("seed recovery source");
    std::fs::write(engine.root().join(".restored.txt.tmp"), b"user bytes")
        .expect("seed similarly named user file");
    symlink(&target, engine.root().join("restored.txt")).expect("symlink leaf");

    let outcome = copy_replacement(
        &engine.root(),
        &wp("restored.txt"),
        &source,
        FileMode::new(0o640),
    )
    .expect("anchored recovery copy");

    assert!(matches!(outcome, GuardedWrite::Blocked));
    assert_eq!(
        std::fs::read(&target).expect("external target"),
        b"external bytes",
        "recovery must never follow the destination symlink"
    );
    assert!(
        is_symlink(&engine.root().join("restored.txt")),
        "the recovery write refuses the symlink leaf"
    );
    assert_eq!(
        engine.read(".restored.txt.tmp"),
        b"user bytes",
        "recovery does not reuse or clobber a user-visible temp sibling"
    );

    std::fs::remove_file(engine.root().join("restored.txt")).expect("remove refused symlink");
    let outcome = copy_replacement(
        &engine.root(),
        &wp("restored.txt"),
        &source,
        FileMode::new(0o640),
    )
    .expect("anchored regular recovery copy");
    assert!(matches!(outcome, GuardedWrite::Written(_)));
    assert_eq!(engine.read("restored.txt"), b"recovered bytes");
    assert_eq!(mode_of(&engine.root().join("restored.txt")), 0o640);
}
