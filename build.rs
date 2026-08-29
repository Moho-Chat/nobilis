use std::path::Path;
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

    // Rebuild when the checkout moves. HEAD is a file in an ordinary clone
    // and lives under the parent's modules directory in a submodule, so
    // whichever is actually there is the one to watch.
    for path in [".git/HEAD", "../.git/modules/nobilis/HEAD"] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
