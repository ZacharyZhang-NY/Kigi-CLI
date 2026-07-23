pub use kigi_sampling_types::types::*;

use kigi_tools::types::output::ImageContent as ToolsImageContent;

/// Render an `ImageContent` produced by the read-file tool as a URL
/// string suitable for an `image_url` content block: passes the
/// explicit `uri` through if present, otherwise builds a
/// `data:<mime>;base64,<data>` URI.
///
/// Lives in the shell (rather than `kigi-sampling-types` or
/// `kigi-tools`) so neither of those crates needs to depend on
/// `agent-client-protocol`.
pub fn get_image_content_url(image_content: &ToolsImageContent) -> String {
    if let Some(uri) = &image_content.uri {
        uri.clone()
    } else {
        format!(
            "data:{};base64,{}",
            image_content.mime_type, image_content.data
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_image_content_url_returns_uri_when_present() {
        let img = ToolsImageContent {
            data: "AAA".to_string(),
            mime_type: "image/png".to_string(),
            uri: Some("https://example.com/image.png".to_string()),
            annotations: None,
            meta: None,
        };
        assert_eq!(get_image_content_url(&img), "https://example.com/image.png");
    }

    #[test]
    fn get_image_content_url_builds_data_uri_when_no_uri() {
        let img = ToolsImageContent {
            data: "VGVzdA==".to_string(),
            mime_type: "image/png".to_string(),
            uri: None,
            annotations: None,
            meta: None,
        };
        assert_eq!(
            get_image_content_url(&img),
            "data:image/png;base64,VGVzdA=="
        );
    }
}
