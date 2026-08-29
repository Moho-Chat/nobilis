//! Keeping private files private, on whichever system this is.
//!
//! nobilis stores real credentials on disk: Discord tokens, IRC and NickServ
//! passwords, Matrix access tokens. Anything that can read `accounts.toml` can
//! sign in as the user everywhere, so how that file is protected is a security
//! decision and not a formatting one.
//!
//! The two systems answer it differently enough that a single call cannot mean
//! the same thing on both, which is exactly why this is stated in one place
//! rather than gated at each call site. A `#[cfg(unix)]` around the mode bits
//! - the obvious way to make this compile on Windows - would leave the Windows
//! build applying *no* protection while reading as though it applied some,
//! which is the worst of the available outcomes.
//!
//! What each system actually does:
//!
//! **Unix** sets the mode explicitly: 0600 for a file, 0700 for a directory.
//! It has to be explicit, because the default is whatever the process umask
//! says, and a permissive umask is common enough that relying on it would be
//! relying on luck.
//!
//! **Windows** inherits the ACL of the containing directory, and everything
//! nobilis writes lives under the user's own profile. Verified on a real
//! Windows 11 guest rather than taken from documentation: the config directory
//! came back as SYSTEM, Administrators and the user, all inherited, and
//! nothing else. So a file created there is already private in the sense 0600
//! means, without this code doing anything - a genuine platform guarantee
//! rather than an assumption made to avoid the work, and why this is not a
//! silent no-op but a documented one.
//!
//! What Windows does *not* get from that is protection from an administrator
//! or from another program running as the same user - and neither does Unix,
//! where root and any process of the same user can read the file too. The real
//! answer on both is to stop storing bearer tokens in a file at all and hand
//! them to the platform's credential store. That is worth doing and is not
//! what this module is.

use anyhow::{Context, Result};
use std::path::Path;

/// Makes a file readable and writable by its owner and nobody else.
///
/// Call after creating or rewriting anything holding a credential.
pub fn restrict_file_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        // Inherited from the user's profile directory - see the module note.
        // Touched so the argument is used and the intent is greppable.
        let _ = path;
    }
    Ok(())
}

/// Makes a directory enterable by its owner and nobody else.
pub fn restrict_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Creates or truncates a file that only its owner may read.
///
/// The mode is set as part of opening on Unix rather than afterwards: writing
/// a credential into a world-readable file and tightening it a moment later
/// leaves a window in which it was readable, and that window is enough.
pub fn create_private_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {} for writing", path.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_private_file_is_created_and_writable() {
        let dir = std::env::temp_dir().join(format!("nobilis-secure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret");
        {
            use std::io::Write;
            let mut f = create_private_file(&path).unwrap();
            f.write_all(b"token").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "token");
        restrict_file_to_owner(&path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a credential file must not be readable by anyone else");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restricting_a_directory_keeps_it_usable_by_its_owner() {
        let dir = std::env::temp_dir().join(format!("nobilis-secure-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        restrict_to_owner(&dir).unwrap();
        // Still ours to write into, which is the point - a directory nobody
        // can enter would be secure and useless.
        std::fs::write(dir.join("inside"), b"x").unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
