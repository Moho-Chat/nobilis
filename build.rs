use std::path::{Path, PathBuf};
use std::process::Command;

/// Stamps the build with the commit it came from.
///
/// So a running daemon can say what it is. "Which build am I on" is otherwise
/// unanswerable from inside the app: an AppImage keeps running from its own
/// mount after the file on disk is replaced, so the thing on screen and the
/// thing in the directory routinely differ.
///
/// A missing or unavailable git is not an error - a source tarball has no
/// repository and still has to build - so the stamp becomes "unknown" rather
/// than failing the compile.
fn main() {
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    // A working tree with changes in it is not the commit it names, and
    // saying so is the whole point of showing a hash at all.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty());
    let stamp = if dirty { format!("{commit}-modified") } else { commit };
    println!("cargo:rustc-env=NOBILIS_BUILD_COMMIT={stamp}");
    watch_head();
}

/// Asks cargo to run this again when the checkout moves.
///
/// Watching HEAD alone is not enough, and quietly so: on a branch its content
/// is "ref: refs/heads/master", which does not change when commits land - the
/// ref file does. Naming only HEAD also *replaces* cargo's default of
/// rebuilding when any file in the package changes, so the stamp froze
/// completely: the code kept updating and the commit it claimed did not.
///
/// So both are named, plus packed-refs, where a ref lives once git has packed
/// it away and the loose file no longer exists.
fn watch_head() {
    let Some(git_dir) = git_dir() else { return };
    let head = git_dir.join("HEAD");
    if !head.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());
    println!("cargo:rerun-if-changed={}", git_dir.join("packed-refs").display());
    if let Some(reference) = std::fs::read_to_string(&head).ok().and_then(|h| h.strip_prefix("ref: ").map(|r| r.trim().to_string())) {
        println!("cargo:rerun-if-changed={}", git_dir.join(reference).display());
    }
}

/// Where this package's git data actually lives.
///
/// A submodule's `.git` is a file holding "gitdir: <path>" rather than a
/// directory, and this crate is built both ways - standalone, and as moho's
/// submodule, which is the copy that gets packaged.
fn git_dir() -> Option<PathBuf> {
    let dot_git = Path::new(".git");
    if dot_git.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    let pointer = std::fs::read_to_string(dot_git).ok()?;
    let path = pointer.strip_prefix("gitdir:")?.trim();
    Some(PathBuf::from(path))
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
