//! The macOS `osascript` attachment probe: scratch files, script text, and the read flow.

use std::path::PathBuf;

use super::attachments_protocol::{FURL_MARKER, IMAGE_MARKER, parse_attachments_output};
use super::{ClipboardAttachments, ImageData};

/// Private 0700 dir, removed on drop: a shared path let a concurrent kigi swap the raster.
pub(crate) struct ProbeTemps(tempfile::TempDir);

impl ProbeTemps {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("kigi-clipboard-probe-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        builder
            .tempdir()
            .map(Self)
            .map_err(|e| anyhow::anyhow!("failed to create clipboard probe dir: {e}"))
    }

    /// PNG, TIFF, JPEG: the AppleScript writes the first class that coerces.
    pub(crate) fn paths(&self) -> (PathBuf, PathBuf, PathBuf) {
        (
            self.0.path().join("probe.png"),
            self.0.path().join("probe.tiff"),
            self.0.path().join("probe.jpg"),
        )
    }
}

/// File URLs first, a raster only when none are present; without `temps` the raster half is skipped.
pub(crate) fn attachments_osascript(temps: Option<&ProbeTemps>) -> String {
    let image_probe = temps.map_or_else(String::new, raster_half);
    format!(
        "set furlOut to \"none\"\n\
         try\n\
         set urlList to the clipboard as list\n\
         set out to \"\"\n\
         repeat with u in urlList\n\
         try\n\
         set itemRef to contents of u as \u{00AB}class furl\u{00BB}\n\
         set out to out & POSIX path of itemRef & \"\\n\"\n\
         end try\n\
         end repeat\n\
         if out is not \"\" then\n\
         set furlOut to out\n\
         else\n\
         try\n\
         set urlRef to the clipboard as \u{00AB}class furl\u{00BB}\n\
         set furlOut to POSIX path of urlRef\n\
         on error\n\
         end try\n\
         end if\n\
         on error\n\
         try\n\
         set urlRef to the clipboard as \u{00AB}class furl\u{00BB}\n\
         set furlOut to POSIX path of urlRef\n\
         on error\n\
         end try\n\
         end try\n\
         set imageOut to \"NONE\"\n\
         {image_probe}\
         return \"{furl_marker}\" & linefeed & furlOut & linefeed & \"{image_marker}\" & linefeed & \"IMAGE:\" & imageOut",
        furl_marker = FURL_MARKER,
        image_marker = IMAGE_MARKER,
    )
}

fn raster_half(temps: &ProbeTemps) -> String {
    let (path_png, path_tiff, path_jpg) = temps.paths();
    format!(
        "if furlOut is \"none\" then\n\
         try\n\
         set imgData to the clipboard as \u{00AB}class PNGf\u{00BB}\n\
         set filePath to POSIX file \"{png}\" as text\n\
         set fRef to open for access file filePath with write permission\n\
         set eof of fRef to 0\n\
         write imgData to fRef\n\
         close access fRef\n\
         set imageOut to \"PNGf\"\n\
         on error\n\
         try\n\
         set imgData to the clipboard as \u{00AB}class TIFF\u{00BB}\n\
         set filePath to POSIX file \"{tiff}\" as text\n\
         set fRef to open for access file filePath with write permission\n\
         set eof of fRef to 0\n\
         write imgData to fRef\n\
         close access fRef\n\
         set imageOut to \"TIFF\"\n\
         on error\n\
         try\n\
         set imgData to the clipboard as \u{00AB}class JPEG\u{00BB}\n\
         set filePath to POSIX file \"{jpg}\" as text\n\
         set fRef to open for access file filePath with write permission\n\
         set eof of fRef to 0\n\
         write imgData to fRef\n\
         close access fRef\n\
         set imageOut to \"JPEG\"\n\
         on error\n\
         end try\n\
         end try\n\
         end try\n\
         end if\n",
        png = path_png.display(),
        tiff = path_tiff.display(),
        jpg = path_jpg.display(),
    )
}

/// The class on stdout picks the file and MIME; an empty file means no raster.
pub(crate) fn read_probe_raster(
    class: &str,
    temps: &ProbeTemps,
) -> anyhow::Result<Option<ImageData>> {
    let (path_png, path_tiff, path_jpg) = temps.paths();
    let (temp_path, mime) = match class {
        "PNGf" => (path_png, "image/png"),
        "TIFF" => (path_tiff, "image/tiff"),
        "JPEG" => (path_jpg, "image/jpeg"),
        _ => return Ok(None),
    };
    let data = std::fs::read(&temp_path)
        .map_err(|e| anyhow::anyhow!("failed to read clipboard temp file: {e}"))?;
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(ImageData {
        data,
        mime_type: mime.to_owned(),
    }))
}

/// `run` executes a script and returns its stdout; a file paste needs no scratch dir.
pub(crate) fn osascript_attachments(
    temps: anyhow::Result<ProbeTemps>,
    run: impl FnOnce(&str) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<ClipboardAttachments> {
    if let Err(error) = &temps {
        tracing::warn!(%error, "clipboard probe reads file URLs only");
    }
    let stdout = run(&attachments_osascript(temps.as_ref().ok()))?;
    let (file_urls, image_class) = parse_attachments_output(&String::from_utf8_lossy(&stdout));
    if file_urls.is_some() {
        return Ok(ClipboardAttachments {
            file_urls,
            image: None,
        });
    }
    let temps = temps?;
    let image = match image_class {
        Some(class) => read_probe_raster(class, &temps)?,
        None => None,
    };
    Ok(ClipboardAttachments {
        file_urls: None,
        image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachments_stdout(furl: &str, image: &str) -> Vec<u8> {
        format!("{FURL_MARKER}\n{furl}\n{IMAGE_MARKER}\nIMAGE:{image}").into_bytes()
    }

    #[test]
    fn two_probes_never_share_a_path() {
        let first = ProbeTemps::new().unwrap();
        let second = ProbeTemps::new().unwrap();
        assert_ne!(first.paths().0, second.paths().0);
    }

    #[test]
    fn scratch_dir_goes_away_with_the_probe() {
        let temps = ProbeTemps::new().unwrap();
        let (png, tiff, jpg) = temps.paths();
        let dir = png.parent().unwrap().to_path_buf();
        assert_eq!(tiff.parent().unwrap(), dir);
        assert_eq!(jpg.parent().unwrap(), dir);
        std::fs::write(&png, b"raster").unwrap();

        drop(temps);
        assert!(!dir.exists(), "{} must be removed", dir.display());
    }

    #[cfg(unix)]
    #[test]
    fn scratch_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let temps = ProbeTemps::new().unwrap();
        let dir = temps.paths().0.parent().unwrap().to_path_buf();
        let mode = std::fs::metadata(dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn reported_class_is_read_from_this_probes_file() {
        let temps = ProbeTemps::new().unwrap();
        let (_png, tiff, _jpg) = temps.paths();
        std::fs::write(&tiff, b"MM\x00\x2a").unwrap();
        let script_names_the_file = tiff.display().to_string();

        let read = osascript_attachments(Ok(temps), |script| {
            assert!(script.contains(&script_names_the_file), "{script}");
            Ok(attachments_stdout("none", "TIFF"))
        })
        .unwrap();

        let image = read.image.expect("raster attached");
        assert_eq!(image.mime_type, "image/tiff");
        assert_eq!(image.data, b"MM\x00\x2a".to_vec());
        assert_eq!(read.file_urls, None);
    }

    #[test]
    fn file_urls_survive_a_missing_scratch_dir() {
        let read = osascript_attachments(Err(anyhow::anyhow!("no scratch dir")), |script| {
            assert!(
                !script.contains("open for access"),
                "raster half must be skipped"
            );
            Ok(attachments_stdout("/tmp/a.txt\n", "NONE"))
        })
        .unwrap();

        assert_eq!(read.file_urls.as_deref(), Some("/tmp/a.txt"));
        assert_eq!(read.image, None);
    }

    #[test]
    fn no_scratch_dir_and_no_file_url_is_an_error_not_an_empty_board() {
        let err = osascript_attachments(Err(anyhow::anyhow!("no scratch dir")), |_| {
            Ok(attachments_stdout("none", "NONE"))
        })
        .unwrap_err();

        assert!(err.to_string().contains("no scratch dir"), "{err}");
    }
}
