// SPDX-License-Identifier: MIT
//! The [`Command`] type and its supporting pieces: `{ id, title, category,
//! args_schema, precondition, handler }` per design.md §9.4.
//!
//! "Everything the app can do is registered here, including plugin-provided
//! commands" -- see `docs/commands.md` for the 307-entry catalogue this
//! registers in Phase 3 (T-3.3.2). This module defines the shape only;
//! Phase 2's job is interfaces that compile, not command bodies.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::id::CommandId;
use crate::predicate::{ContextState, Predicate};

/// A human-readable grouping label for the command palette, e.g.
/// `"Navigation"`, `"File operations"`, `"Archives"` (see the category
/// column in `docs/commands.md`).
///
/// Left as an open string rather than a closed `enum` because plugins
/// contribute their own categories (FR-PLUG-02) and the built-in list is
/// itself large (23 categories at last count) and not this crate's business
/// to hard-code.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandCategory(Box<str>);

impl CommandCategory {
    /// Wraps a category label.
    pub fn new(label: impl Into<Box<str>>) -> Self {
        Self(label.into())
    }

    /// Borrows the category label.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommandCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for CommandCategory {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// The primitive value kinds a command argument can hold.
///
/// Mirrors `docs/config-schema.md`'s `binding.args` ("command-specific, per
/// its `args_schema`", e.g. `args = { mask = "*.rs" }`) and
/// `docs/commands.md`'s `tab.goto_index` example (`args_schema: { index:
/// u32 }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArgKind {
    String,
    Integer,
    Float,
    Bool,
    /// A filesystem path, distinguished from a plain string so a future
    /// keymap editor / palette can offer path completion.
    Path,
}

/// One field in a command's declared argument schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ArgField {
    pub name: Box<str>,
    pub kind: ArgKind,
    pub required: bool,
    /// A short human-readable description, shown in the keymap editor / a
    /// future `duet command describe` CLI.
    pub description: Box<str>,
}

impl ArgField {
    /// Builds a required argument field.
    pub fn required(
        name: impl Into<Box<str>>,
        kind: ArgKind,
        description: impl Into<Box<str>>,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            required: true,
            description: description.into(),
        }
    }

    /// Builds an optional argument field.
    pub fn optional(
        name: impl Into<Box<str>>,
        kind: ArgKind,
        description: impl Into<Box<str>>,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            required: false,
            description: description.into(),
        }
    }
}

/// A command's declared argument shape.
///
/// `docs/commands.md` notes `args_schema` is "decided in T-2.4.1" and out of
/// scope for the catalogue itself -- this is that decision: a command
/// either takes no arguments, or declares a fixed set of named, typed
/// fields that both `binding.args` (fixed, keymap-supplied) and the palette
/// (interactively prompted) validate against.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ArgsSchema {
    /// The command takes no arguments, e.g. `ops.copy`, `nav.parent`.
    #[default]
    None,
    /// The command takes the given named, typed fields, e.g.
    /// `tab.goto_index`'s `{ index: u32 }`.
    Fields(Vec<ArgField>),
}

/// A concrete argument value supplied at invocation time (from a keymap
/// binding's `args` table, a palette prompt, or a plugin call).
#[derive(Debug, Clone, PartialEq)]
pub enum ArgValue {
    String(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    Path(std::path::PathBuf),
}

/// A fully-resolved set of arguments passed to a command invocation, keyed
/// by [`ArgField::name`].
pub type CommandArgs = HashMap<String, ArgValue>;

/// The state a command handler executes against: whatever surface currently
/// has focus (a panel, a dialog, the palette, ...), the selection, and a way
/// to evaluate further predicates. Kept intentionally minimal and
/// UI-framework-agnostic in this crate; `duet-ui` supplies the real
/// implementation, and `duet-ops`/`duet-index` supply the model access a
/// concrete handler needs.
///
/// This is a placeholder shape for Phase 2: a real `CommandContext` needs
/// access to the active `DirectoryModel` (T-2.5.1), the operation queue
/// (T-2.3.x), and more, none of which exist yet on this branch. The trait
/// exists now so [`CommandHandler`]'s signature is stable for callers to
/// build against.
pub trait CommandContext {
    /// The context-predicate state visible to this invocation (used to
    /// re-check `precondition` right before dispatch, and by handlers that
    /// need to branch on UI state).
    fn context_state(&self) -> &dyn ContextState;
}

/// Why a command invocation failed.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// The command's `precondition` did not hold at dispatch time.
    #[error("command {0} is not available in the current context")]
    PreconditionFailed(CommandId),
    /// A required argument was missing, or a supplied argument didn't match
    /// `args_schema`.
    #[error("invalid arguments for command {command}: {message}")]
    InvalidArgs { command: CommandId, message: String },
    /// The handler itself failed. Boxed to keep this enum's size small and
    /// to avoid coupling `duet-commands` to every downstream crate's error
    /// type; a real handler wraps its own error here.
    #[error("command {command} failed: {source}")]
    HandlerFailed {
        command: CommandId,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// The outcome of dispatching a command.
pub type CommandResult = Result<(), CommandError>;

/// A command handler: the function invoked when the command fires, given
/// the invocation's resolved arguments and a [`CommandContext`].
///
/// Wrapped in `Arc` (not `Box`) so a registered [`Command`] can be cheaply
/// cloned out of the registry for dispatch without holding the registry
/// borrowed for the duration of the call -- important once command
/// execution can itself register/unregister commands (plugin
/// enable/disable).
type HandlerFn = dyn Fn(&mut dyn CommandContext, &CommandArgs) -> CommandResult + Send + Sync;

#[derive(Clone)]
pub struct CommandHandler(Arc<HandlerFn>);

impl CommandHandler {
    /// Wraps a closure/function as a [`CommandHandler`].
    pub fn new(
        f: impl Fn(&mut dyn CommandContext, &CommandArgs) -> CommandResult + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(f))
    }

    /// Invokes the handler. Real dispatch (matching a key/palette selection
    /// to a `Command`, re-checking `precondition`, resolving `args`) is
    /// T-3.3.2's job; this method is the seam it will call through.
    pub fn call(&self, ctx: &mut dyn CommandContext, args: &CommandArgs) -> CommandResult {
        (self.0)(ctx, args)
    }
}

impl fmt::Debug for CommandHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandHandler")
            .field("fn", &"<closure>")
            .finish()
    }
}

/// A registered command: `{ id, title, category, args_schema, precondition,
/// handler }`, verbatim per design.md §9.4.
///
/// One `Command` value exists per entry in `docs/commands.md`, plus one per
/// plugin-registered command (`plugin.command.<plugin-id>.<name>` etc., per
/// that document's "Plugins" section).
#[derive(Clone)]
pub struct Command {
    pub id: CommandId,
    /// The display title shown in the palette and keymap editor, e.g.
    /// `"Copy selection to the target panel"`.
    pub title: Box<str>,
    pub category: CommandCategory,
    pub args_schema: ArgsSchema,
    /// The context predicate that must hold for this command to be
    /// available (e.g. `panel && selection.nonempty`). Re-checked at
    /// dispatch time, not just used to grey out UI.
    pub precondition: Predicate,
    pub handler: CommandHandler,
}

impl Command {
    /// Convenience builder for the common case of a command with no
    /// argument schema.
    pub fn new(
        id: CommandId,
        title: impl Into<Box<str>>,
        category: impl Into<CommandCategory>,
        precondition: Predicate,
        handler: CommandHandler,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            category: category.into(),
            args_schema: ArgsSchema::None,
            precondition,
            handler,
        }
    }

    /// Returns whether this command is currently available, i.e. whether
    /// `precondition` holds against `state`.
    pub fn is_available(&self, state: &dyn ContextState) -> bool {
        self.precondition.eval(state)
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("category", &self.category)
            .field("args_schema", &self.args_schema)
            .field("precondition", &self.precondition.to_string())
            .field("handler", &self.handler)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::{MapContextState, parse};

    struct NullContext<'a>(&'a dyn ContextState);
    impl CommandContext for NullContext<'_> {
        fn context_state(&self) -> &dyn ContextState {
            self.0
        }
    }

    #[test]
    fn command_is_available_reflects_precondition() {
        let cmd = Command::new(
            CommandId::new("ops.copy").unwrap(),
            "Copy selection to the target panel",
            "File operations",
            parse("panel && selection.nonempty").unwrap(),
            CommandHandler::new(|_ctx, _args| Ok(())),
        );

        let mut state = MapContextState::new();
        assert!(!cmd.is_available(&state));

        state
            .set_flag("panel", true)
            .set_flag("selection.nonempty", true);
        assert!(cmd.is_available(&state));
    }

    #[test]
    fn handler_invokes_closure() {
        let cmd = Command::new(
            CommandId::new("app.quit").unwrap(),
            "Quit",
            "Application",
            parse("app").unwrap(),
            CommandHandler::new(|_ctx, _args| Ok(())),
        );
        let state = MapContextState::new();
        let mut ctx = NullContext(&state);
        assert!(cmd.handler.call(&mut ctx, &CommandArgs::new()).is_ok());
    }
}
