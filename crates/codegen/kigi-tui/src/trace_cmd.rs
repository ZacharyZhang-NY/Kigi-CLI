use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kigi_shell::agent::config::Config as AgentConfig;
use kigi_shell::util::kigi_home::kigi_home;

#[derive(Debug, clap::Args, Clone)]
pub struct TraceArgs {
    /// Session ID to export
    pub session_id: String,
    /// Output path (default: $KIGI_SHARE_DIR/trace-exports/<session-id>.tar.gz)
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Emit machine-readable JSON output
    #[arg(long)]
    pub json: bool,
}

#[derive(serde::Serialize)]
struct TraceResult {
    session_id: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn run(args: TraceArgs, _agent_config: &AgentConfig) -> Result<()> {
    run_export(&args.session_id, args.output.as_deref(), args.json).await
}

// ---------------------------------------------------------------------------
// Archive construction
// ---------------------------------------------------------------------------

pub fn build_session_tar(session_dir: &Path, session_id: &str) -> Result<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    tracing::info!(
        session_id = %session_id,
        session_dir = %session_dir.display(),
        "trace_cmd: building session tar.gz archive"
    );

    let mut archive_data = Vec::new();
    let mut file_count: u32 = 0;
    {
        let encoder = GzEncoder::new(&mut archive_data, Compression::default());
        let mut archive = tar::Builder::new(encoder);

        file_count += add_directory_to_tar(&mut archive, session_dir, session_id)?;

        let metadata = ExportMetadata {
            session_id: session_id.to_owned(),
            kigi_version: env!("VERSION_WITH_COMMIT").to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            exported_at: chrono::Utc::now().to_rfc3339(),
        };
        let meta_bytes = serde_json::to_vec_pretty(&metadata)?;
        append_bytes(
            &mut archive,
            &format!("{session_id}/export_metadata.json"),
            &meta_bytes,
        );
        file_count += 1;

        archive
            .into_inner()
            .and_then(|encoder| encoder.finish())
            .context("Failed to finalize tar.gz archive")?;
    }

    tracing::info!(
        session_id = %session_id,
        file_count,
        archive_bytes = archive_data.len(),
        "trace_cmd: archive built"
    );

    Ok(archive_data)
}

#[derive(serde::Serialize)]
struct ExportMetadata {
    session_id: String,
    kigi_version: String,
    os: String,
    arch: String,
    exported_at: String,
}

fn append_bytes<W: std::io::Write>(archive: &mut tar::Builder<W>, path: &str, data: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    set_mtime(&mut header);
    if let Err(e) = archive.append_data(&mut header, path, data) {
        tracing::warn!(error = %e, "trace_cmd: failed to add file to archive");
        eprintln!("  Warning: failed to add {path}: {e}");
    }
}

fn set_mtime(header: &mut tar::Header) {
    header.set_mtime(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    );
}

/// Returns the number of files added.
fn add_directory_to_tar<W: std::io::Write>(
    archive: &mut tar::Builder<W>,
    dir: &Path,
    prefix: &str,
) -> Result<u32> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("Failed to read {}", dir.display()))?;

    let mut count: u32 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let archive_path = format!("{prefix}/{name_str}");

        if path.is_dir() {
            count += add_directory_to_tar(archive, &path, &archive_path)?;
        } else if path.is_file() {
            match std::fs::read(&path) {
                Ok(data) => {
                    append_bytes(archive, &archive_path, &data);
                    count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "trace_cmd: failed to read file for archive"
                    );
                    eprintln!("  Warning: failed to read {}: {}", path.display(), e);
                }
            }
        }
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// Local export
// ---------------------------------------------------------------------------

pub(crate) fn find_session_dir(session_id: &str) -> Result<PathBuf> {
    kigi_shell::session::persistence::find_session_dir_by_id(session_id).with_context(|| {
        format!(
            "Session '{session_id}' not found under {}",
            crate::util::display_user_kigi_path("sessions")
        )
    })
}

pub fn trace_exports_dir() -> PathBuf {
    kigi_home().join("trace-exports")
}

/// Creates parent directory if needed.
pub fn save_local_bundle(
    archive: &[u8],
    session_id: &str,
    output: Option<&Path>,
) -> Result<PathBuf> {
    let output_path = match output {
        Some(p) => p.to_path_buf(),
        None => trace_exports_dir().join(format!("{session_id}.tar.gz")),
    };

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    std::fs::write(&output_path, archive)
        .with_context(|| format!("Failed to write {}", output_path.display()))?;

    tracing::info!(
        session_id = %session_id,
        path = %output_path.display(),
        size_bytes = archive.len(),
        "trace_cmd: local bundle saved"
    );

    Ok(output_path)
}

async fn run_export(session_id: &str, output: Option<&Path>, json: bool) -> Result<()> {
    let session_dir = find_session_dir(session_id)?;
    if !json {
        eprintln!("Found session at: {}", session_dir.display());
        eprintln!("Building session trace archive...");
    }

    let archive = build_session_tar(&session_dir, session_id)?;
    let output_path = save_local_bundle(&archive, session_id, output)?;

    if json {
        let result = TraceResult {
            session_id: session_id.to_owned(),
            status: "exported",
            local_path: Some(output_path.display().to_string()),
            error: None,
        };
        println!("{}", serde_json::to_string(&result)?);
    } else {
        let size_kb = archive.len() / 1024;
        eprintln!("Session trace exported ({size_kb} KB):");
        eprintln!("  {}", output_path.display());
        println!("{}", output_path.display());
    }
    Ok(())
}
