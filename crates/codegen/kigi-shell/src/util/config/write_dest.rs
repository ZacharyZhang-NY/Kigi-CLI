//! Where a config write lands when the path is a symlink.

use std::io;
use std::path::{Path, PathBuf};

/// The user's own config.toml keeps its symlink; every other path comes back unchanged.
pub(crate) fn config_write_dest(path: &Path) -> io::Result<PathBuf> {
    // With no home, `kigi_home()` is `./.kigi`: the project's own tree, never trusted.
    let user_config = kigi_config::user_kigi_home().map(|home| home.join("config.toml"));
    write_dest(path, user_config.as_deref())
}

/// A project config is repo-controlled: following its symlink would overwrite any file it names.
fn write_dest(path: &Path, user_config: Option<&Path>) -> io::Result<PathBuf> {
    if user_config != Some(path) {
        return Ok(path.to_path_buf());
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => dunce::canonicalize(path).map_err(|e| {
            let context = format!("config symlink {} does not resolve: {e}", path.display());
            io::Error::new(e.kind(), context)
        }),
        Ok(_) => Ok(path.to_path_buf()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(e) => Err(e),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::util::config::{McpServerConfig, save_mcp_server_config_at};

    fn symlinked_config(dir: &Path, referent_content: &str) -> (PathBuf, PathBuf) {
        let referent = dir.join("dotfiles.toml");
        std::fs::write(&referent, referent_content).unwrap();
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&referent, &link).unwrap();
        (link, referent)
    }

    #[test]
    fn user_config_symlink_resolves_to_its_referent() {
        let dir = tempfile::tempdir().unwrap();
        let (link, referent) = symlinked_config(dir.path(), "");

        let dest = write_dest(&link, Some(&link)).unwrap();
        assert_eq!(dest, dunce::canonicalize(&referent).unwrap());
    }

    #[test]
    fn regular_and_missing_user_config_keep_their_path() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("config.toml");
        std::fs::write(&regular, "").unwrap();
        let missing = dir.path().join("absent.toml");

        assert_eq!(write_dest(&regular, Some(&regular)).unwrap(), regular);
        assert_eq!(write_dest(&missing, Some(&missing)).unwrap(), missing);
    }

    #[test]
    fn dangling_user_config_symlink_is_an_error_not_a_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(dir.path().join("gone.toml"), &link).unwrap();

        let err = write_dest(&link, Some(&link)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("does not resolve"), "{err}");
    }

    #[test]
    fn project_config_symlink_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let (link, _referent) = symlinked_config(dir.path(), "");
        let user_config = dir.path().join("home").join("config.toml");

        assert_eq!(write_dest(&link, Some(&user_config)).unwrap(), link);
    }

    #[test]
    fn nothing_is_followed_when_no_user_home_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let (link, _referent) = symlinked_config(dir.path(), "");

        assert_eq!(write_dest(&link, None).unwrap(), link);
    }

    #[tokio::test]
    async fn project_scope_save_replaces_the_link_and_spares_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let (link, target) = symlinked_config(dir.path(), "victim = true\n");

        let server: McpServerConfig =
            toml::from_str("command = \"echo\"\nargs = [\"hi\"]\n").unwrap();
        save_mcp_server_config_at(&link, "demo", &server)
            .await
            .unwrap();

        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(
            meta.file_type().is_file(),
            "the link is replaced, not followed"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "victim = true\n");
        let saved = std::fs::read_to_string(&link).unwrap();
        assert!(saved.contains("[mcp_servers.demo]"), "{saved}");
    }
}
