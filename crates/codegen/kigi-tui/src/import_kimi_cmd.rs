//! `kigi import-kimi` — one-time, read-only import of the official kimi-cli
//! configuration (`~/.kimi`), PRD F7.
//!
//! Reads `~/.kimi/config.toml` + `~/.kimi/mcp.json` without modifying them
//! and merges MCP servers, custom model providers, and the default-model
//! preference into `~/.kigi/config.toml`. Existing kigi entries are never
//! overwritten. A marker file under the kigi home makes the import one-time.

use anyhow::Result;

pub fn run(dry_run: bool) -> Result<()> {
    if kigi_shell::kimi_import::is_kimi_import_marked() {
        println!(
            "kimi-cli settings were already imported (marker: {}).\n\
             Delete the marker file and re-run to import again.",
            kigi_shell::kimi_import::kimi_import_marker_path().display()
        );
        return Ok(());
    }
    let Some(plan) = kigi_shell::kimi_import::scan()? else {
        println!("Nothing to import: no importable settings found in ~/.kimi.");
        return Ok(());
    };
    print!("{}", plan.summary());
    if dry_run {
        println!("\nDry run: nothing was written.");
        return Ok(());
    }
    let applied = kigi_shell::kimi_import::apply(&plan)?;
    println!();
    print!("{}", applied.summary());
    Ok(())
}
