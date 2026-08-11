// Stub fs-plugin -- T-2.6.1 WIT bindgen validation, see stub-content for
// the fuller rationale comment.

wit_bindgen::generate!({
    path: "../../wit",
    world: "fs-plugin-world",
});

use duet::plugin::host;
use duet::plugin::types::Entry;
use exports::duet::plugin::fs_plugin::{BackendCaps, Guest};

struct Component;

impl Guest for Component {
    fn connect(_config: String) -> Result<host::Handle, host::Error> {
        host::log(host::Level::Info, "stub-fs: connect()");
        Ok(1)
    }

    fn disconnect(_session: host::Handle) -> Result<(), host::Error> {
        Ok(())
    }

    fn capabilities(_session: host::Handle) -> BackendCaps {
        BackendCaps {
            atomic_replace: false,
            server_side_copy: false,
            server_side_move: false,
            symlinks: false,
        }
    }

    fn list_dir(_session: host::Handle, _dir: String) -> Result<Vec<Entry>, host::Error> {
        let _ = host::progress(0, 1);
        Ok(vec![])
    }

    fn stat(_session: host::Handle, path: String) -> Result<Entry, host::Error> {
        Ok(Entry {
            name: path,
            size: 0,
            mtime: 0,
            mode: 0,
            is_dir: false,
            is_symlink: false,
        })
    }

    fn get(_session: host::Handle, _path: String, dest: host::Handle) -> Result<(), host::Error> {
        let s = host::open_granted(dest).map_err(|_| host::Error::Unsupported)?;
        s.write(&[]).map_err(|_| host::Error::Unsupported)?;
        Ok(())
    }

    fn put(_session: host::Handle, _path: String, src: host::Handle) -> Result<(), host::Error> {
        let s = host::open_granted(src).map_err(|_| host::Error::Unsupported)?;
        let _ = s.read(0).map_err(|_| host::Error::Unsupported)?;
        Ok(())
    }

    fn remove(_session: host::Handle, _path: String, _recursive: bool) -> Result<(), host::Error> {
        Err(host::Error::Unsupported)
    }

    fn mkdir(_session: host::Handle, _path: String) -> Result<(), host::Error> {
        Err(host::Error::Unsupported)
    }

    fn rename(_session: host::Handle, _old_path: String, _new_path: String) -> Result<(), host::Error> {
        Err(host::Error::Unsupported)
    }
}

export!(Component);
