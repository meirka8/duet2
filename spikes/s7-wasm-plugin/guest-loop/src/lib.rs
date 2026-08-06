wit_bindgen::generate!({
    path: "../wit",
    world: "content-host",
});

use exports::duet::plugin_spike::content_plugin::{FieldDef, FieldValue, Guest};

struct Component;

impl Guest for Component {
    fn fields() -> Vec<FieldDef> {
        vec![FieldDef {
            name: "runaway".to_string(),
            kind: "integer".to_string(),
        }]
    }

    /// Deliberately misbehaving: this is the "runaway plugin" half of the
    /// S-7 spike. It never returns on its own -- the host must stop it via
    /// fuel/epoch interruption (design.md §9.9, FR-PLUG-06).
    #[allow(clippy::empty_loop)]
    fn value(_path: String, _field: u32) -> Result<FieldValue, String> {
        let mut counter: u64 = 0;
        loop {
            // Prevent the optimizer from proving this loop has no
            // observable effect and folding it away or trapping early;
            // the wrapping add keeps real work (and real epoch/fuel
            // check back-edges) in the compiled loop body.
            counter = counter.wrapping_add(1);
            std::hint::black_box(counter);
        }
    }
}

export!(Component);
