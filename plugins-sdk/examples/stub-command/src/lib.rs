// Stub command-plugin -- T-2.6.1 WIT bindgen validation, see stub-content
// for the fuller rationale comment.

wit_bindgen::generate!({
    path: "../../wit",
    world: "command-plugin-world",
});

use duet::plugin::host;
use exports::duet::plugin::command_plugin::{CommandDef, Guest, SelectionEntry};

struct Component;

impl Guest for Component {
    fn commands() -> Vec<CommandDef> {
        vec![CommandDef {
            id: "hello".to_string(),
            title: "Stub: say hello".to_string(),
            category: "stub".to_string(),
        }]
    }

    fn invoke(
        id: String,
        selection: Vec<SelectionEntry>,
        content: Vec<Option<host::Handle>>,
    ) -> Result<(), host::Error> {
        host::log(
            host::Level::Info,
            &format!(
                "stub-command: invoke({id}) on {} selected entries",
                selection.len()
            ),
        );
        for h in content.into_iter().flatten() {
            let s = host::open_granted(h).map_err(|_| host::Error::Unsupported)?;
            let _ = s.read(0).map_err(|_| host::Error::Unsupported)?;
        }
        Ok(())
    }
}

export!(Component);
