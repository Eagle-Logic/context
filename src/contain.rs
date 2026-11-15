//! Root containment: the one guarantee `SECURITY.md` states as a vulnerability
//! if broken.
//!
//! > The MCP server pins itself to a root (its working directory unless
//! > `--root` says otherwise) and refuses a `path` that resolves outside it,
//! > including via `..` or a symlink. A way around that is a vulnerability.
//!
//! This lives here, taking its root as an argument, rather than in `mcp.rs`
//! where it reads a process-global `OnceLock`. The guarantee is then a PURE
//! FUNCTION of (root, requested path) — which is what makes it fuzzable, and a
//! guarantee nothing can exercise adversarially is a guarantee on trust.

use std::path::{Path, PathBuf};

/// Resolve `requested` against `root`, refusing anything that escapes it.
///
/// Canonicalization happens BEFORE the comparison, which is the whole trick:
/// `..` segments and symlinks are resolved to a real location first, so neither
/// can be used to leave the tree. Comparing the unresolved path would accept
/// `root/../../etc/passwd` on the grounds that it starts with `root`.
///
/// A path that does not exist is refused rather than accepted, because
/// canonicalization is what proves containment and there is nothing to
/// canonicalize. That also means the function cannot be used to probe for the
/// existence of files outside the root: both answers are an error, and the
/// messages differ only in naming the root the caller already knows.
///
/// `Path::starts_with` is component-wise, not textual. `/srv/project` does not
/// contain `/srv/project-secrets`, which a string prefix would have accepted.
pub fn contained(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let joined = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        root.join(requested)
    };
    let real = joined
        .canonicalize()
        .map_err(|e| format!("cannot resolve path {requested:?}: {e}"))?;
    if !real.starts_with(root) {
        return Err(format!(
            "refused: {requested:?} resolves to {} which is outside this server's root ({}). \
             The MCP server only reads the project it was started in. Start it with \
             `ctx mcp --root <dir>` to point it somewhere else.",
            real.display(),
            root.display()
        ));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A root with a file in it, an `outside` sibling, and a symlink from the
    /// first to the second. Returns (root, outside).
    fn fixture() -> (PathBuf, PathBuf) {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("ctx_contain_{}_{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("inside.rs"), "fn a() {}\n").unwrap();
        fs::write(outside.join("secret.rs"), "fn b() {}\n").unwrap();
        (
            root.canonicalize().unwrap(),
            outside.canonicalize().unwrap(),
        )
    }

    #[test]
    fn a_path_inside_the_root_resolves() {
        let (root, _) = fixture();
        let got = contained(&root, "inside.rs").expect("an in-root path must resolve");
        assert!(got.starts_with(&root));
    }

    #[test]
    fn dot_dot_traversal_is_refused() {
        let (root, _) = fixture();
        let err = contained(&root, "../outside/secret.rs").expect_err("`..` escaped the root");
        assert!(err.contains("refused"), "{err}");
    }

    /// Named explicitly by SECURITY.md. The symlink lives INSIDE the root, so
    /// an unresolved comparison would accept it.
    #[test]
    #[cfg(unix)]
    fn a_symlink_out_of_the_root_is_refused() {
        let (root, outside) = fixture();
        let link = root.join("escape");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let err = contained(&root, "escape/secret.rs").expect_err("a symlink escaped the root");
        assert!(err.contains("refused"), "{err}");
        // And via the link directory itself, not just a file under it.
        assert!(contained(&root, "escape").is_err());
    }

    /// A string prefix would accept this; a component-wise one must not.
    #[test]
    fn a_sibling_sharing_a_name_prefix_is_refused() {
        let (root, _) = fixture();
        let sibling = root.with_file_name("root-secrets");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("s.rs"), "fn c() {}\n").unwrap();
        let err = contained(&root, sibling.join("s.rs").to_str().unwrap())
            .expect_err("a name-prefix sibling was treated as inside");
        assert!(err.contains("refused"), "{err}");
    }

    #[test]
    fn a_path_that_does_not_exist_is_refused_not_accepted() {
        let (root, _) = fixture();
        assert!(contained(&root, "no/such/file.rs").is_err());
    }
}
