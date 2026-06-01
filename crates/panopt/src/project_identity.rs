//! Resolve a project's stable **identity** at the edge.
//!
//! A project's identity and its *projection location* are two different things
//! (note #123, todo #124). The projection location is a filesystem path - the
//! checkout whose `.panopt/*.md` mirror we write - and it is inherently
//! path-bound. Identity is not: the same repo cloned to another path, moved,
//! checked out as a worktree, or mirrored on another machine is still one
//! logical project. So identity is resolved here, at the launcher / agent-config
//! edge, and passed to the daemon as an opaque `project=` key; `panopt-core`
//! stores it without ever knowing it came from git, which keeps the git
//! dependency out of core (a workspace boundary, see CLAUDE.md).
//!
//! Identity cannot be *derived* from git alone. A fork shares the upstream's
//! entire history, so the root-commit hash is identical for both - deriving from
//! it would merge a fork into its upstream. "Same project?" is human intent, not
//! data. So the resolver is a fallback chain beneath an explicit, committed
//! declaration:
//!
//! 1. `.panopt/project-id` committed in the tree -> authoritative. This is how a
//!    fork declares itself separate (`panopt project init` rewrites it) and how
//!    anything opts into an id that travels with the tree.
//! 2. else the normalized remote origin URL ("same push target = same project").
//! 3. else the root-commit hash (a local repo with no remote).
//! 4. else the canonical filesystem path (not a git repo) - today's behavior.
//!
//! Rule 4 emits the **raw canonical path**, with no prefix or decoration, so the
//! key matches exactly what the V10 schema migration back-filled for existing
//! path-keyed rows (`identity = root`). A path-only project therefore keeps its
//! identity across the upgrade.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Which resolver rule produced an identity key. Carried alongside the key so
/// callers (and tests) can see *why* a project was identified the way it was -
/// useful for diagnostics and for surfacing "this is a git-remote project" vs
/// "this is a bare path" in the eventual switcher UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// A `.panopt/project-id` committed in the tree.
    Declared,
    /// The normalized `git remote get-url origin`.
    Remote,
    /// The root-commit hash (`git rev-list --max-parents=0 HEAD`).
    RootCommit,
    /// The canonical filesystem path (not a git repo).
    Path,
}

/// A resolved project identity: the opaque `key` to pass as `project=`, plus the
/// `source` rule that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    pub key: String,
    pub source: IdentitySource,
}

/// Resolve the identity of the project rooted at `dir`, walking the fallback
/// chain documented on this module. Always succeeds: the path fallback (rule 4)
/// has no failure mode beyond canonicalization, which itself falls back to the
/// path as given.
pub fn resolve(dir: &Path) -> ProjectIdentity {
    if let Some(key) = read_declared(dir) {
        return ProjectIdentity {
            key,
            source: IdentitySource::Declared,
        };
    }
    if let Some(key) = git(dir, &["remote", "get-url", "origin"]).and_then(|u| normalize_remote(&u))
    {
        return ProjectIdentity {
            key,
            source: IdentitySource::Remote,
        };
    }
    if let Some(out) = git(dir, &["rev-list", "--max-parents=0", "HEAD"]) {
        // A repo can have more than one root commit (merged histories); take the
        // first line so the key is deterministic regardless of how many there are.
        if let Some(key) = out.lines().next().filter(|l| !l.is_empty()) {
            return ProjectIdentity {
                key: key.to_string(),
                source: IdentitySource::RootCommit,
            };
        }
    }
    ProjectIdentity {
        key: canonical_path_key(dir),
        source: IdentitySource::Path,
    }
}

/// Path to the committed identity file for the project rooted at `dir`. The
/// single source of this location, shared with `panopt project init` which
/// writes it (see `project.rs`).
pub fn id_file(dir: &Path) -> PathBuf {
    dir.join(".panopt").join("project-id")
}

/// Read a committed `.panopt/project-id`, trimmed. `None` if absent or blank.
fn read_declared(dir: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(id_file(dir)).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Run `git -C <dir> <args>` and return trimmed stdout, or `None` if git is
/// absent, the command fails, or the output is empty. Errors are swallowed on
/// purpose: every git rule is optional and falls through to the next.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The canonical absolute path as a string, falling back to the path as given
/// if canonicalization fails (e.g. a component no longer exists). This is the
/// exact form the V10 migration back-filled, so path-keyed projects resolve to
/// the same identity before and after the upgrade.
fn canonical_path_key(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| PathBuf::from(dir))
        .to_string_lossy()
        .into_owned()
}

/// Normalize a git remote URL into a stable key: strip the scheme, any
/// `userinfo@` credentials, and a trailing `.git`; fold an scp-style
/// `host:path` separator into `host/path`; lowercase the host (paths stay
/// case-sensitive). `None` for a blank/degenerate URL so the caller falls
/// through to the next rule.
///
/// Examples (all collapse to `github.com/acme/widget`):
/// - `https://github.com/acme/widget.git`
/// - `git@github.com:acme/widget.git`
/// - `ssh://git@GitHub.com/acme/widget`
fn normalize_remote(url: &str) -> Option<String> {
    let mut s = url.trim();
    if s.is_empty() {
        return None;
    }
    // Strip a `scheme://` prefix.
    if let Some(idx) = s.find("://") {
        s = &s[idx + 3..];
    }
    // Strip `userinfo@` credentials from the authority.
    if let Some(idx) = s.find('@') {
        s = &s[idx + 1..];
    }
    let mut s = s.to_string();
    // scp-style `host:path` -> `host/path`, but only when the first ':' precedes
    // any '/' (otherwise it is a path component or, rarely, a `host:port`).
    if let Some(colon) = s.find(':') {
        if s.find('/').is_none_or(|slash| colon < slash) {
            s.replace_range(colon..=colon, "/");
        }
    }
    // Drop trailing slashes and a trailing `.git`.
    let trimmed = s.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    // Lowercase the host (everything up to the first '/'); leave the path alone.
    let key = match trimmed.split_once('/') {
        Some((host, rest)) => format!("{}/{rest}", host.to_ascii_lowercase()),
        None => trimmed.to_ascii_lowercase(),
    };
    (!key.is_empty()).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A throwaway directory under the system temp dir, removed on drop. Avoids
    /// pulling in a `tempfile` dependency just for these tests.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("panopt-idtest-{}-{tag}-{seq}", std::process::id()));
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

    /// Run git in `dir` with global/system config neutralized and identity
    /// pinned, so commits succeed in any CI environment. Returns whether git ran.
    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn normalize_remote_folds_the_common_url_shapes() {
        let want = Some("github.com/acme/widget".to_string());
        assert_eq!(normalize_remote("https://github.com/acme/widget.git"), want);
        assert_eq!(normalize_remote("git@github.com:acme/widget.git"), want);
        assert_eq!(normalize_remote("ssh://git@GitHub.com/acme/widget"), want);
        assert_eq!(
            normalize_remote("https://user:pass@github.com/acme/widget"),
            want
        );
        assert_eq!(normalize_remote("HTTPS://github.com/acme/widget/"), want);
        // Host lowercased, path case preserved.
        assert_eq!(
            normalize_remote("git@Example.COM:Acme/Widget.git"),
            Some("example.com/Acme/Widget".to_string())
        );
        // Degenerate input falls through.
        assert_eq!(normalize_remote("   "), None);
    }

    #[test]
    fn declared_id_file_wins() {
        let dir = TempDir::new("declared");
        std::fs::create_dir_all(dir.path().join(".panopt")).unwrap();
        std::fs::write(dir.path().join(".panopt/project-id"), "  my-uuid-123\n").unwrap();

        let got = resolve(dir.path());
        assert_eq!(got.source, IdentitySource::Declared);
        assert_eq!(got.key, "my-uuid-123");
    }

    #[test]
    fn non_git_dir_falls_back_to_canonical_path() {
        let dir = TempDir::new("path");
        let got = resolve(dir.path());
        assert_eq!(got.source, IdentitySource::Path);
        // Matches what the V10 migration back-fills for path-keyed rows.
        assert_eq!(got.key, canonical_path_key(dir.path()));
    }

    #[test]
    fn git_remote_then_root_commit_then_path() {
        let dir = TempDir::new("git");
        if !git_ok(dir.path(), &["init", "-q"]) {
            eprintln!("skipping: git unavailable");
            return;
        }
        std::fs::write(dir.path().join("f"), "x").unwrap();
        assert!(git_ok(dir.path(), &["add", "."]));
        assert!(git_ok(dir.path(), &["commit", "-q", "-m", "init"]));

        // With a remote, the normalized origin URL wins over the root commit.
        assert!(git_ok(
            dir.path(),
            &["remote", "add", "origin", "git@github.com:acme/widget.git"]
        ));
        let got = resolve(dir.path());
        assert_eq!(got.source, IdentitySource::Remote);
        assert_eq!(got.key, "github.com/acme/widget");

        // Drop the remote: now identity is the root-commit hash (40 hex chars).
        assert!(git_ok(dir.path(), &["remote", "remove", "origin"]));
        let got = resolve(dir.path());
        assert_eq!(got.source, IdentitySource::RootCommit);
        assert_eq!(got.key.len(), 40);
        assert!(got.key.chars().all(|c| c.is_ascii_hexdigit()));

        // A committed project-id still trumps git.
        std::fs::create_dir_all(dir.path().join(".panopt")).unwrap();
        std::fs::write(dir.path().join(".panopt/project-id"), "declared-wins").unwrap();
        let got = resolve(dir.path());
        assert_eq!(got.source, IdentitySource::Declared);
        assert_eq!(got.key, "declared-wins");
    }
}
