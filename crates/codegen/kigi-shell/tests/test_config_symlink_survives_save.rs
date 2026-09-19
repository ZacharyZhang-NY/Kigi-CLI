//! A symlinked `~/.kigi/config.toml` survives a save: the write lands on the referent.
#![cfg(unix)]

use kigi_shell::util::config::{McpServerConfig, save_mcp_server_config};

#[tokio::test]
async fn user_config_symlink_survives_a_save() {
    let home = tempfile::tempdir().unwrap();
    let dotfiles = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before `kigi_home()` first runs.
    unsafe { std::env::set_var("KIGI_SHARE_DIR", home.path()) };

    let referent = dotfiles.path().join("kigi.toml");
    std::fs::write(&referent, "[ui]\ntheme = \"dark\"\n").unwrap();
    let link = home.path().join("config.toml");
    std::os::unix::fs::symlink(&referent, &link).unwrap();

    let server: McpServerConfig = toml::from_str("command = \"echo\"\nargs = [\"hi\"]\n").unwrap();
    save_mcp_server_config("demo", &server).await.unwrap();

    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(meta.file_type().is_symlink(), "the link must survive");
    let saved = std::fs::read_to_string(&referent).unwrap();
    assert!(saved.contains("[mcp_servers.demo]"), "{saved}");
    assert!(saved.contains("theme = \"dark\""), "{saved}");
    let leftovers: Vec<_> = std::fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "config.toml")
        .collect();
    assert!(
        leftovers.is_empty(),
        "no tmp beside the link: {leftovers:?}"
    );
}
