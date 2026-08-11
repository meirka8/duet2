// Stub content-plugin: validates that `content-plugin-world` (WIT for
// T-2.6.1) produces guest bindings that a real component can implement and
// compile against, including calling back into the imported `host`
// interface (not just exporting). Not a reference plugin -- see
// task.md T-8.1.9 for that; this exists to satisfy T-2.6.1's AC
// ("wit-bindgen generates host and guest bindings; a stub plugin compiles
// against them").

wit_bindgen::generate!({
    path: "../../wit",
    world: "content-plugin-world",
});

use duet::plugin::host;
use exports::duet::plugin::content_plugin::{FieldDef, FieldKind, FieldValue, Guest, Target};

struct Component;

impl Guest for Component {
    fn fields() -> Vec<FieldDef> {
        vec![
            FieldDef {
                name: "stub-name-len".to_string(),
                kind: FieldKind::Integer,
            },
            FieldDef {
                name: "stub-content-head-len".to_string(),
                kind: FieldKind::Integer,
            },
        ]
    }

    fn value(target: Target, field: u32) -> Result<FieldValue, host::Error> {
        host::log(host::Level::Debug, "stub-content: value() called");
        match field {
            // Field 0 needs no granted content -- pure metadata.
            0 => Ok(FieldValue::Integer(target.meta.name.len() as i64)),
            // Field 1 exercises the resource-typed open-granted() path: no
            // ambient path/fd, just a handle exchanged for a stream via the
            // imported `host` interface -- the part S-7's spike didn't
            // cover (it had zero host imports by design).
            1 => match target.content {
                Some(h) => {
                    let s = host::open_granted(h)
                        .map_err(|_| host::Error::Unsupported)?;
                    let head = s.read(4).map_err(|_| host::Error::Unsupported)?;
                    Ok(FieldValue::Integer(head.len() as i64))
                }
                None => Ok(FieldValue::None),
            },
            _ => {
                let _ = host::progress(0, 1);
                let _ = host::secret_get("unused");
                Err(host::Error::InvalidArgument("unknown field".to_string()))
            }
        }
    }
}

export!(Component);
