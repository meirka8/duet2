// SPDX-License-Identifier: MIT
//! [`CommandRegistry`]: the single table every command in the app --
//! built-in and plugin-provided -- is registered into (design.md §9.4:
//! "Everything the app can do is registered here").

use std::collections::HashMap;

use crate::command::Command;
use crate::id::CommandId;

/// Raised when registering a command whose id already has an entry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("command id {0} is already registered")]
pub struct DuplicateCommandError(pub CommandId);

/// The command registry: a flat `CommandId -> Command` table.
///
/// Every consumer of "what can the app do" -- the command palette
/// (FR-TOOL-11), the keymap resolver (binds a key to a registered id, T-3.3.2
/// AC "200 commands register"), and the rebinding UI (FR-CFG-02) -- reads
/// from this one structure. Plugins extend it at load time through the same
/// `register` call built-ins use (FR-PLUG-02), so there is no special-cased
/// "plugin command" path here -- see `docs/commands.md`'s Plugins section
/// for how plugin ids are namespaced (`plugin.command.<plugin-id>.<name>`).
#[derive(Default)]
pub struct CommandRegistry {
    commands: HashMap<CommandId, Command>,
    /// Preserves registration order so palette listings and `iter()` are
    /// deterministic (and stable across runs for snapshot-style tests),
    /// rather than at the mercy of `HashMap` iteration order.
    order: Vec<CommandId>,
}

impl CommandRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a command. Errors if `command.id` is already registered --
    /// silent overwrite would let a buggy plugin shadow a built-in command
    /// (or another plugin) without anyone noticing, the same "silently
    /// shadowed" failure mode §9.4 calls out for keymap bindings.
    pub fn register(&mut self, command: Command) -> Result<(), DuplicateCommandError> {
        if self.commands.contains_key(&command.id) {
            return Err(DuplicateCommandError(command.id.clone()));
        }
        self.order.push(command.id.clone());
        self.commands.insert(command.id.clone(), command);
        Ok(())
    }

    /// Removes a registered command (e.g. on plugin disable/uninstall,
    /// FR-PLUG-06). Returns the removed command, if any.
    pub fn unregister(&mut self, id: &CommandId) -> Option<Command> {
        let removed = self.commands.remove(id);
        if removed.is_some() {
            self.order.retain(|existing| existing != id);
        }
        removed
    }

    /// Looks up a command by id.
    pub fn get(&self, id: &CommandId) -> Option<&Command> {
        self.commands.get(id)
    }

    /// Returns whether a command id is currently registered.
    pub fn contains(&self, id: &CommandId) -> bool {
        self.commands.contains_key(id)
    }

    /// The number of registered commands.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the registry has no commands.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Iterates registered commands in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &Command> {
        self.order
            .iter()
            .filter_map(move |id| self.commands.get(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CommandHandler;
    use crate::predicate::parse;

    fn cmd(id: &str) -> Command {
        Command::new(
            CommandId::new(id).unwrap(),
            id.to_string(),
            "Test",
            parse("app").unwrap(),
            CommandHandler::new(|_ctx, _args| Ok(())),
        )
    }

    #[test]
    fn register_and_get() {
        let mut reg = CommandRegistry::new();
        reg.register(cmd("ops.copy")).unwrap();
        assert!(reg.get(&CommandId::new("ops.copy").unwrap()).is_some());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn duplicate_registration_errors() {
        let mut reg = CommandRegistry::new();
        reg.register(cmd("ops.copy")).unwrap();
        let err = reg.register(cmd("ops.copy")).unwrap_err();
        assert_eq!(err.0, CommandId::new("ops.copy").unwrap());
    }

    #[test]
    fn unregister_removes_and_returns() {
        let mut reg = CommandRegistry::new();
        reg.register(cmd("ops.copy")).unwrap();
        let removed = reg.unregister(&CommandId::new("ops.copy").unwrap());
        assert!(removed.is_some());
        assert!(reg.is_empty());
    }

    #[test]
    fn iter_preserves_registration_order() {
        let mut reg = CommandRegistry::new();
        for id in ["nav.parent", "ops.copy", "app.quit"] {
            reg.register(cmd(id)).unwrap();
        }
        let ids: Vec<_> = reg.iter().map(|c| c.id.as_str().to_string()).collect();
        assert_eq!(ids, vec!["nav.parent", "ops.copy", "app.quit"]);
    }
}
