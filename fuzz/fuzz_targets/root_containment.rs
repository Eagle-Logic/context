#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

/// A root with things worth escaping through: a nested directory, a symlink out
/// to a sibling the root must never reach, and a sibling whose name shares the
/// root's prefix (which a textual comparison would wrongly accept).
///
/// Built once. The fuzzer mutates the PATH STRING, not the tree — the tree is
/// the adversarial part of the environment, held fixed so every input is
/// attacking the same known layout.
fn fixture() -> &'static (PathBuf, PathBuf) {
    static F: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    F.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("ctx_fuzz_contain_{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("in.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("nested/deep.rs"), "fn b() {}\n").unwrap();
        std::fs::write(outside.join("secret.rs"), "fn c() {}\n").unwrap();
        // A sibling sharing the root's name prefix.
        std::fs::create_dir_all(base.join("root-secrets")).unwrap();
        std::fs::write(base.join("root-secrets/s.rs"), "fn d() {}\n").unwrap();
        #[cfg(unix)]
        {
            let link = root.join("escape");
            if !link.exists() {
                let _ = std::os::unix::fs::symlink(&outside, &link);
            }
        }
        (
            root.canonicalize().unwrap(),
            outside.canonicalize().unwrap(),
        )
    })
}

fuzz_target!(|data: &[u8]| {
    // A path is an OS string; non-UTF-8 is a separate question and the MCP
    // layer only ever hands this function a JSON string, which is UTF-8.
    let Ok(requested) = std::str::from_utf8(data) else {
        return;
    };
    let (root, outside) = fixture();

    // The guarantee, stated as a property: EITHER refused, OR inside the root.
    // There is no third outcome, for any input whatsoever.
    match code_context::contain::contained(root, requested) {
        Err(_) => {}
        Ok(p) => {
            assert!(
                p.starts_with(root),
                "contained() accepted {requested:?} which resolves to {} — outside the root {}",
                p.display(),
                root.display()
            );
            assert!(
                !p.starts_with(outside),
                "contained() accepted a path into the off-limits tree: {requested:?} -> {}",
                p.display()
            );
        }
    }
});
