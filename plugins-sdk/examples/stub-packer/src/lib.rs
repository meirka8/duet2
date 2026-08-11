// Stub packer-plugin -- T-2.6.1 WIT bindgen validation, see stub-content
// for the fuller rationale comment.

wit_bindgen::generate!({
    path: "../../wit",
    world: "packer-plugin-world",
});

use duet::plugin::host;
use duet::plugin::types::Entry;
use exports::duet::plugin::packer_plugin::Guest;

struct Component;

impl Guest for Component {
    fn probe(head: Vec<u8>, name: String) -> bool {
        host::log(host::Level::Debug, "stub-packer: probe()");
        name.ends_with(".stubzip") && head.first() == Some(&b'S')
    }

    fn list_members(archive: host::Handle) -> Result<Vec<Entry>, host::Error> {
        let s = host::open_granted(archive).map_err(|_| host::Error::Unsupported)?;
        let _head = s.read(4).map_err(|_| host::Error::Unsupported)?;
        Ok(vec![])
    }

    fn extract(_archive: host::Handle, _member: String, out: host::Handle) -> Result<(), host::Error> {
        let s = host::open_granted(out).map_err(|_| host::Error::Unsupported)?;
        s.write(&[]).map_err(|_| host::Error::Unsupported)?;
        Ok(())
    }

    fn can_write() -> bool {
        false
    }

    fn add(_archive: host::Handle, _member: String, _src: host::Handle) -> Result<(), host::Error> {
        Err(host::Error::Unsupported)
    }
}

export!(Component);
