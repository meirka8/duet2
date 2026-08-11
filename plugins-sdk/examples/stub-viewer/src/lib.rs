// Stub viewer-plugin -- T-2.6.1 WIT bindgen validation, see stub-content
// for the fuller rationale comment.

wit_bindgen::generate!({
    path: "../../wit",
    world: "viewer-plugin-world",
});

use duet::plugin::host;
use exports::duet::plugin::viewer_plugin::{Guest, Surface};

struct Component;

impl Guest for Component {
    fn probe(head: Vec<u8>, name: String) -> bool {
        name.ends_with(".stubviewer") && !head.is_empty()
    }

    fn render_to_markdown(content: host::Handle) -> Result<String, host::Error> {
        let s = host::open_granted(content).map_err(|_| host::Error::Unsupported)?;
        let bytes = s.read(64).map_err(|_| host::Error::Unsupported)?;
        Ok(format!("stub preview: {} bytes read", bytes.len()))
    }

    fn render_to_surface(
        _content: host::Handle,
        max_width: u32,
        max_height: u32,
    ) -> Result<Surface, host::Error> {
        let w = max_width.min(1);
        let h = max_height.min(1);
        Ok(Surface {
            width: w,
            height: h,
            rgba8: vec![0u8; (w * h * 4) as usize],
        })
    }
}

export!(Component);
