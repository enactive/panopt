//! `panopt project` - manage a project's committed identity.
//!
//! PANopt keys a project on a stable repo identity, not its checkout path (note
//! #123, todo #124). When a tree carries a `.panopt/project-id`, that file is
//! authoritative and travels with the tree across clones, moves, worktrees, and
//! machines - so two checkouts of one repo coordinate as one project, and a fork
//! that wants to be its own project just declares a new id. `panopt project init`
//! is how that file gets written: it is the one intentional command that says
//! "this tree is (or is no longer) the same project as its neighbors".
//!
//! Without an `init`, identity is still resolved automatically (remote URL ->
//! root commit -> path; see [`crate::project_identity`]); `init` only matters
//! when the automatic answer is wrong - most often a fork, which shares the
//! upstream's whole history and so cannot be told apart from it by git alone.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::project_identity::{self, IdentitySource};

#[derive(Subcommand)]
pub enum ProjectCmd {
    /// Write (or rewrite) `.panopt/project-id` with a fresh identity, declaring
    /// this tree a distinct project. Commit the file so the identity travels
    /// with the tree; rewrite it in a fork to split the fork off from upstream.
    Init,
}

/// Entry point for `panopt project <cmd>`.
pub fn run(ws: Option<PathBuf>, cmd: ProjectCmd) -> Result<()> {
    let ws = resolve_ws(ws)?;
    match cmd {
        ProjectCmd::Init => init_cmd(&ws),
    }
}

/// `panopt project init` - declare (or re-declare) this tree's identity.
fn init_cmd(ws: &Path) -> Result<()> {
    // Capture what identity resolved to *before* we write, so we can tell the
    // user whether this replaced an existing committed id or overrode a derived
    // one (a remote / root-commit / path key).
    let previous = project_identity::resolve(ws);
    let id = write_identity(ws)?;
    let path = project_identity::id_file(ws);

    println!("wrote {} = {id}", path.display());
    match previous.source {
        IdentitySource::Declared => {
            println!("  (replaced the previous committed id {})", previous.key);
        }
        other => {
            println!(
                "  (was resolving to {} via {})",
                previous.key,
                source_label(other)
            );
        }
    }
    ensure_committable(ws, &path)?;
    println!(
        "commit it so the identity travels with the tree:\n    git add {} && git commit -m 'panopt: declare project id'",
        path.display()
    );
    Ok(())
}

/// Make sure git will actually let the freshly-written id be committed.
///
/// The id lives under `.panopt/`, which most repos ignore. The `!project-id`
/// negation in `.panopt/.gitignore` only works when no *ancestor* `.gitignore`
/// excludes the `.panopt/` directory outright - git cannot re-include a file
/// whose parent directory is excluded. When such a directory-exclusion is the
/// reason the id is still ignored, rewrite that one rule from a directory
/// exclusion (`.panopt/`) into a contents exclusion plus a re-include
/// (`.panopt/*` + `!.panopt/project-id`), the minimal edit that lets the id
/// through while still ignoring the rest of the projection. Anything more
/// exotic (a global ignore, a `*` rule) we leave to the user with a clear note.
fn ensure_committable(ws: &Path, id_path: &Path) -> Result<()> {
    let Some((file, pattern)) = ignoring_rule(ws) else {
        return Ok(()); // not a git repo, no matching rule, or already re-included
    };
    if [".panopt/", ".panopt", "/.panopt/", "/.panopt"].contains(&pattern.as_str()) {
        repair_dir_exclusion(&file, &pattern)?;
        println!(
            "  adjusted {} so the id is committable: `{pattern}` -> `.panopt/*` + `!.panopt/project-id`",
            file.display()
        );
    } else {
        println!(
            "  note: git still ignores this file via `{pattern}` in {}.\n  \
             commit it with `git add -f {}`, or allow `.panopt/project-id` in that file.",
            file.display(),
            id_path.display()
        );
    }
    Ok(())
}

/// If `.panopt/project-id` is currently ignored, return the `(gitignore file,
/// pattern)` responsible. `None` when it is not ignored - either no rule matches
/// or a negation re-includes it - or when `ws` is not a git repo / git is absent.
fn ignoring_rule(ws: &Path) -> Option<(PathBuf, String)> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["check-ignore", "-v", ".panopt/project-id"])
        .output()
        .ok()?;
    // `-v` prints `<file>:<lineno>:<pattern>\t<path>` for the last matching rule,
    // a negation included. A leading `!` means the path is re-included (already
    // committable), so there is nothing to repair.
    let stdout = String::from_utf8(output.stdout).ok()?;
    let line = stdout.lines().next()?;
    let mut parts = line.splitn(4, [':', '\t']);
    let file = parts.next()?;
    let _lineno = parts.next()?;
    let pattern = parts.next()?.trim().to_string();
    if pattern.starts_with('!') {
        return None;
    }
    // `file` is reported relative to `ws` (we ran `git -C ws`); anchor it.
    Some((ws.join(file), pattern))
}

/// Rewrite the single `.gitignore` line that excludes the `.panopt/` directory
/// into a contents-exclusion plus a re-include of the id, preserving every other
/// line. Only the first line whose trimmed text equals `pattern` is changed.
fn repair_dir_exclusion(file: &Path, pattern: &str) -> Result<()> {
    let content =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let anchor = if pattern.starts_with('/') { "/" } else { "" };
    let mut out = String::with_capacity(content.len() + 32);
    let mut replaced = false;
    for line in content.lines() {
        if !replaced && line.trim() == pattern {
            out.push_str(&format!("{anchor}.panopt/*\n{anchor}!.panopt/project-id\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    std::fs::write(file, out).with_context(|| format!("writing {}", file.display()))
}

/// Create `.panopt/`, keep git from ignoring the id, and write a fresh UUID.
/// Returns the id written. Factored out from [`init_cmd`] for tests.
fn write_identity(ws: &Path) -> Result<String> {
    let dir = ws.join(".panopt");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    // The daemon's projection bootstrap writes `.panopt/.gitignore` as
    // `*\n!project-id\n`; mirror that here so `init` works before the daemon has
    // ever touched the project, and so the committed id is not swallowed by the
    // blanket ignore. Only (re)write when the negation is missing, to avoid
    // stomping a `.gitignore` a user has customized.
    let gitignore = dir.join(".gitignore");
    let has_negation = std::fs::read_to_string(&gitignore)
        .map(|c| c.lines().any(|l| l.trim() == "!project-id"))
        .unwrap_or(false);
    if !has_negation {
        std::fs::write(&gitignore, "*\n!project-id\n")
            .with_context(|| format!("writing {}", gitignore.display()))?;
    }

    let id = uuid_v4();
    let path = project_identity::id_file(ws);
    std::fs::write(&path, format!("{id}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// A random UUIDv4 string from the system RNG. PANopt never parses the id back -
/// it is an opaque key - so any stable random token would do; a canonical UUID
/// just reads as obviously-an-identity to a human inspecting the tree.
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("reading /dev/urandom");
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 1 (RFC 4122)
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
    )
}

/// Human label for an identity source, for the `init` summary line.
fn source_label(source: IdentitySource) -> &'static str {
    match source {
        IdentitySource::Declared => "a committed project-id",
        IdentitySource::Remote => "the remote origin URL",
        IdentitySource::RootCommit => "the root-commit hash",
        IdentitySource::Path => "the filesystem path",
    }
}

/// The project root: the given path, or the current directory, canonicalized.
fn resolve_ws(ws: Option<PathBuf>) -> Result<PathBuf> {
    let ws = match ws {
        Some(ws) => ws,
        None => std::env::current_dir().context("reading the current directory")?,
    };
    std::fs::canonicalize(&ws).with_context(|| format!("no such directory: {}", ws.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Throwaway temp dir, removed on drop - avoids a `tempfile` dependency.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("panopt-projinit-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn init_writes_uuid_negates_ignore_and_resolves_as_declared() {
        let dir = TempDir::new();

        let id = write_identity(dir.path()).unwrap();
        // Canonical UUIDv4 shape: 36 chars, version 4, RFC-4122 variant.
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4', "version nibble");
        assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));

        // The id file holds exactly the id (trimmed), and the ignore negates it.
        let stored = std::fs::read_to_string(project_identity::id_file(dir.path())).unwrap();
        assert_eq!(stored.trim(), id);
        let ignore = std::fs::read_to_string(dir.path().join(".panopt/.gitignore")).unwrap();
        assert!(ignore.contains("!project-id"), "{ignore}");

        // The resolver now treats the tree as declared, with this exact key.
        let resolved = project_identity::resolve(dir.path());
        assert_eq!(resolved.source, IdentitySource::Declared);
        assert_eq!(resolved.key, id);

        // A re-init mints a fresh id (how a fork splits off) and resolve follows.
        let id2 = write_identity(dir.path()).unwrap();
        assert_ne!(id, id2);
        assert_eq!(project_identity::resolve(dir.path()).key, id2);
    }

    #[test]
    fn write_identity_preserves_a_customized_gitignore_with_the_negation() {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.path().join(".panopt")).unwrap();
        // A user-customized ignore that already keeps the id tracked.
        std::fs::write(
            dir.path().join(".panopt/.gitignore"),
            "*\n!project-id\n!notes.md\n",
        )
        .unwrap();

        write_identity(dir.path()).unwrap();

        // Left untouched because it already negates project-id.
        let ignore = std::fs::read_to_string(dir.path().join(".panopt/.gitignore")).unwrap();
        assert_eq!(ignore, "*\n!project-id\n!notes.md\n");
    }

    #[test]
    fn repair_dir_exclusion_rewrites_only_the_matched_line() {
        let dir = TempDir::new();
        let gi = dir.path().join(".gitignore");
        std::fs::write(&gi, "/target\n.panopt/\n.DS_Store\n").unwrap();

        repair_dir_exclusion(&gi, ".panopt/").unwrap();

        assert_eq!(
            std::fs::read_to_string(&gi).unwrap(),
            "/target\n.panopt/*\n!.panopt/project-id\n.DS_Store\n"
        );
    }

    /// Run git in `dir` with config neutralized; returns whether it succeeded.
    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn init_repairs_a_root_gitignore_that_excludes_the_panopt_dir() {
        let dir = TempDir::new();
        if !git_ok(dir.path(), &["init", "-q"]) {
            eprintln!("skipping: git unavailable");
            return;
        }
        // The exact situation from the field: the repo root excludes `.panopt/`
        // as a directory, so a negation *inside* `.panopt/` cannot re-include the
        // id - git never descends into an excluded directory.
        std::fs::write(dir.path().join(".gitignore"), "/target\n.panopt/\n").unwrap();

        let id = write_identity(dir.path()).unwrap();
        // Before repair, git ignores the id (the directory exclusion wins).
        assert!(ignoring_rule(dir.path()).is_some());

        ensure_committable(dir.path(), &project_identity::id_file(dir.path())).unwrap();

        // After repair the root rule is a contents-exclusion + re-include, and
        // git no longer ignores the id (nothing left to fix).
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "/target\n.panopt/*\n!.panopt/project-id\n"
        );
        assert!(
            ignoring_rule(dir.path()).is_none(),
            "id should be committable after repair"
        );
        // And `git add` accepts it without -f.
        assert!(git_ok(
            dir.path(),
            &["add", "--dry-run", ".panopt/project-id"]
        ));
        let _ = id;
    }
}
