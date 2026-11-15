#![no_main]

use std::path::PathBuf;

use libfuzzer_sys::fuzz_target;

/// Fuzz the whole pipeline, not one extractor: the filesystem walk, language
/// dispatch, and above all RESOLUTION — candidate generation, package scoping,
/// re-export chasing. `fuzz/README.md` named that as the largest uncovered
/// area, on the grounds that most of the logic lives there and none of it was
/// being attacked.
///
/// Slower per iteration than the extractor targets, because each case writes a
/// small tree and walks it. That is the cost of covering the part that needs a
/// whole tree to mean anything.
///
/// The input is split on a NUL byte into alternating name and content. Names
/// are sanitised to a safe relative path: the point is to fuzz the ANALYSER,
/// not to discover that a fuzzer can ask to write to `/etc`.
fn tree_from(data: &[u8]) -> Option<(PathBuf, usize)> {
    let text = std::str::from_utf8(data).ok()?;
    let base = std::env::temp_dir().join(format!(
        "ctx_fuzz_graph_{}_{}",
        std::process::id(),
        // Content-addressed, so concurrent iterations cannot collide.
        code_context::model::content_hash(text)
    ));
    let _ = std::fs::remove_dir_all(&base);

    let mut parts = text.split('\0');
    let mut written = 0usize;
    while let (Some(name), Some(body)) = (parts.next(), parts.next()) {
        // One path component, from a conservative alphabet, with an extension
        // ctx actually parses. Anything else is a filesystem test, not a ctx
        // test.
        let stem: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(24)
            .collect();
        if stem.is_empty() {
            continue;
        }
        let ext = match written % 6 {
            0 => "rs",
            1 => "py",
            2 => "ts",
            3 => "go",
            4 => "java",
            _ => "md",
        };
        let dir = if written % 3 == 0 {
            base.join("src")
        } else {
            base.join("src").join("sub")
        };
        if std::fs::create_dir_all(&dir).is_err() {
            break;
        }
        if std::fs::write(dir.join(format!("{stem}.{ext}")), body).is_err() {
            break;
        }
        written += 1;
        // A cap keeps one iteration cheap enough to be worth running.
        if written >= 8 {
            break;
        }
    }
    if written == 0 {
        let _ = std::fs::remove_dir_all(&base);
        return None;
    }
    Some((base, written))
}

fuzz_target!(|data: &[u8]| {
    let Some((base, _)) = tree_from(data) else {
        return;
    };
    if let Ok(g) = code_context::extract::build_graph(&base) {
        for m in &g.modules {
            assert!(!m.name.is_empty(), "a module with no name reached the graph");

            // The invariant `Diagnostics` documents: every call site seen is
            // accounted for exactly once, as resolved, provably external, or a
            // genuine miss. If these stop summing, every recall figure the
            // project publishes is computed over a denominator that does not
            // describe the tree.
            let d = &m.diag;
            assert_eq!(
                d.call_sites,
                d.resolved + d.external + d.unresolved,
                "module {} loses call sites: {} != {} + {} + {}",
                m.name,
                d.call_sites,
                d.resolved,
                d.external,
                d.unresolved
            );
            assert!(
                d.heuristic <= d.resolved,
                "module {}: {} heuristic edges out of {} resolved",
                m.name,
                d.heuristic,
                d.resolved
            );
            assert!(
                d.dispatch <= d.resolved,
                "module {}: {} dispatch edges out of {} resolved",
                m.name,
                d.dispatch,
                d.resolved
            );
        }
    }
    let _ = std::fs::remove_dir_all(&base);
});
