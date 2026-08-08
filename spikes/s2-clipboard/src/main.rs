//! S-2 spike: clipboard with custom MIME types.
//!
//! GPUI's `ClipboardItem` API (see documentation/spikes/S-2.md) cannot express
//! `text/uri-list` or the GNOME/KDE cut markers -- it only has String/Image
//! variants, and its Linux/Wayland backend only ever offers/reads plain-text
//! MIME types for the clipboard (text/uri-list is used there only for
//! drag-and-drop, a separate code path). This binary is the Wayland-fallback
//! prototype: it talks to `wl_data_device` directly via smithay-client-toolkit.

mod fixtures;
mod mime;
mod read_mode;
mod write_mode;

use mime::Mode;

fn main() {
    let mut args = std::env::args();
    let _argv0 = args.next();
    match args.next().as_deref() {
        Some("write-copy") => write_mode::run(Mode::Copy),
        Some("write-cut") => write_mode::run(Mode::Cut),
        Some("read") => read_mode::run(),
        other => {
            eprintln!("usage: s2_clipboard <write-copy|write-cut|read>");
            if let Some(other) = other {
                eprintln!("  unrecognized subcommand: {other}");
            }
            std::process::exit(2);
        }
    }
}
