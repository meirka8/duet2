wit_bindgen::generate!({
    path: "../wit",
    world: "content-host",
});

use exports::duet::plugin_spike::content_plugin::{FieldDef, FieldValue, Guest};

struct Component;

impl Guest for Component {
    fn fields() -> Vec<FieldDef> {
        vec![FieldDef {
            name: "spike-len".to_string(),
            kind: "integer".to_string(),
        }]
    }

    /// Deliberately trivial: field 0 returns the byte length of `path` as an
    /// integer. The point of this component is the call boundary, not the
    /// computation -- see spikes/s7-wasm-plugin/README (or S-7.md) for why.
    fn value(path: String, field: u32) -> Result<FieldValue, String> {
        match field {
            0 => Ok(FieldValue::Integer(path.len() as i64)),
            _ => Err("unknown field".to_string()),
        }
    }
}

export!(Component);
