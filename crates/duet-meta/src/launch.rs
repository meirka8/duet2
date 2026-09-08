// SPDX-License-Identifier: MIT
//! Launching (T-5.3.4): turning a desktop entry, a command line or an
//! executable file into a detached child process, with terminal
//! applications wrapped in the user's terminal emulator -- the
//! `gio launch` semantics design.md §9.8 asks for, without the D-Bus
//! activation half (`DBusActivatable=true` entries are launched through
//! their `Exec` line like any other; GLib does the same when the bus
//! name isn't owned, and the result is identical for a file manager).
//!
//! [`LaunchPlan`] is the pure description (argv, working directory,
//! whether it wants a terminal, a label for messages); [`spawn`]
//! executes it: the child gets its own session (`setsid`) and process
//! group so it outlives Duet and never receives Duet's terminal
//! signals, and its standard streams go to `/dev/null` -- a GUI
//! application started from a file manager must not inherit the
//! manager's stdio. The caller reaps the returned `Child`.
//!
//! Terminal wrapping: `$TERMINAL` if set, else `xdg-terminal-exec`
//! (the XDG terminal-exec proposal, where installed), else the desktop's
//! configured terminal (`gsettings` on GNOME), else the first of a
//! well-known list on `$PATH`. A script run "in a terminal" is wrapped
//! so the window stays open until Enter after it exits -- otherwise a
//! script that prints and quits flashes a window for a frame, which is
//! what Total Commander avoids with its own "keep window open" wrapper.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::desktop::{DesktopEntry, expand_exec, program_exists};

/// What to run -- see the module doc comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// `argv[0]` is the program (absolute or resolved through `$PATH`).
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Run inside a terminal emulator.
    pub terminal: bool,
    /// Human-readable label for notices ("Document Viewer", "run.sh").
    pub label: String,
}

impl LaunchPlan {
    /// The plan a desktop entry gives for `files`.
    pub fn for_entry(entry: &DesktopEntry, files: &[PathBuf]) -> Option<Self> {
        let argv = expand_exec(&entry.exec, files, entry);
        if argv.is_empty() {
            return None;
        }
        Some(Self {
            argv,
            cwd: entry.working_dir.clone(),
            terminal: entry.terminal,
            label: entry.name.clone(),
        })
    }

    /// The plan for a user-written command in `Exec` syntax (the
    /// `[associations.overrides]` values): field codes expand exactly as
    /// for an entry; `%c`/`%i`/`%k` have nothing to refer to and vanish.
    pub fn for_command(command: &str, files: &[PathBuf]) -> Option<Self> {
        let pseudo = DesktopEntry {
            id: String::new(),
            path: PathBuf::new(),
            name: String::new(),
            exec: command.to_string(),
            try_exec: None,
            working_dir: None,
            terminal: false,
            icon: None,
            no_display: false,
            mime_types: Vec::new(),
        };
        let mut argv = expand_exec(command, files, &pseudo);
        argv.retain(|a| !a.is_empty());
        if argv.is_empty() {
            return None;
        }
        let label = argv[0].clone();
        Some(Self {
            argv,
            cwd: files
                .first()
                .and_then(|f| f.parent().map(Path::to_path_buf)),
            terminal: false,
            label,
        })
    }

    /// The plan for running `path` itself (an executable file), in its
    /// own directory; `in_terminal` for scripts whose output the user
    /// will want to see.
    pub fn for_executable(path: &Path, in_terminal: bool) -> Self {
        Self {
            argv: vec![path.to_string_lossy().into_owned()],
            cwd: path.parent().map(Path::to_path_buf),
            terminal: in_terminal,
            label: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
        }
    }

    /// Whether `argv[0]` can be found.
    pub fn program_available(&self) -> bool {
        self.argv.first().is_some_and(|p| program_exists(p))
    }
}

/// A terminal emulator and how it takes a command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLauncher {
    /// The emulator plus the flag after which the command follows
    /// (`["kgx", "--"]`, `["konsole", "-e"]`).
    pub prefix: Vec<String>,
}

/// Well-known emulators and their "run this command" syntax, tried in
/// order when nothing configured the terminal.
const KNOWN_TERMINALS: &[(&str, &str)] = &[
    ("kgx", "--"),
    ("gnome-terminal", "--"),
    ("ptyxis", "--"),
    ("konsole", "-e"),
    ("xfce4-terminal", "-x"),
    ("alacritty", "-e"),
    ("kitty", "--"),
    ("foot", "--"),
    ("wezterm", "start --"),
    ("tilix", "-e"),
    ("terminator", "-x"),
    ("mate-terminal", "--"),
    ("lxterminal", "-e"),
    ("urxvt", "-e"),
    ("xterm", "-e"),
];

impl TerminalLauncher {
    /// The launcher for a named emulator, if it is installed.
    fn for_program(program: &str) -> Option<Self> {
        let name = Path::new(program)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.to_string());
        if !program_exists(program) {
            return None;
        }
        let flag = KNOWN_TERMINALS
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, flag)| *flag)
            .unwrap_or("-e");
        let mut prefix = vec![program.to_string()];
        prefix.extend(flag.split_whitespace().map(String::from));
        Some(Self { prefix })
    }

    /// Finds the user's terminal -- see the module doc comment. `None`
    /// when no emulator at all is installed.
    pub fn detect() -> Option<Self> {
        if let Some(term) = std::env::var("TERMINAL")
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            && let Some(launcher) = Self::for_program(&term)
        {
            return Some(launcher);
        }
        if program_exists("xdg-terminal-exec") {
            return Some(Self {
                prefix: vec!["xdg-terminal-exec".to_string()],
            });
        }
        if let Ok(output) = Command::new("gsettings")
            .args([
                "get",
                "org.gnome.desktop.default-applications.terminal",
                "exec",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            && output.status.success()
        {
            let raw = String::from_utf8_lossy(&output.stdout);
            let program = raw.trim().trim_matches('\'').trim();
            if let Some(launcher) = Self::for_program(program) {
                return Some(launcher);
            }
        }
        KNOWN_TERMINALS
            .iter()
            .find_map(|(program, _)| Self::for_program(program))
    }

    /// Wraps `argv` so it runs inside this terminal and, when
    /// `hold_open`, waits for Enter after the command exits.
    pub fn wrap(&self, argv: &[String], hold_open: bool) -> Vec<String> {
        let mut out = self.prefix.clone();
        if hold_open {
            out.push("sh".to_string());
            out.push("-c".to_string());
            out.push(
                "\"$0\" \"$@\"; status=$?; printf '\\n[exit %s] Press Enter to close.' \"$status\"; read -r _"
                    .to_string(),
            );
            out.extend(argv.iter().cloned());
        } else {
            out.extend(argv.iter().cloned());
        }
        out
    }
}

/// Why a launch didn't happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// The program isn't installed (or the plan was empty).
    ProgramNotFound(String),
    /// The plan wants a terminal and none could be found.
    NoTerminal,
    /// The OS refused to start it.
    Spawn(String),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::ProgramNotFound(program) => write!(f, "program not found: {program}"),
            LaunchError::NoTerminal => write!(f, "no terminal emulator found (set $TERMINAL)"),
            LaunchError::Spawn(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Starts `plan` detached -- see the module doc comment. The `Child` is
/// returned for the caller to reap (`wait`) off the UI thread.
pub fn spawn(plan: &LaunchPlan) -> Result<Child, LaunchError> {
    let Some(program) = plan.argv.first() else {
        return Err(LaunchError::ProgramNotFound(String::new()));
    };
    if !program_exists(program) {
        return Err(LaunchError::ProgramNotFound(program.clone()));
    }
    let argv: Vec<String> = if plan.terminal {
        let terminal = TerminalLauncher::detect().ok_or(LaunchError::NoTerminal)?;
        terminal.wrap(&plan.argv, true)
    } else {
        plan.argv.clone()
    };
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = plan.cwd.as_ref().filter(|d| d.is_dir()) {
        command.current_dir(cwd);
    }
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
        // SAFETY: `setsid` is async-signal-safe and touches no memory
        // shared with the parent; it only detaches the child from Duet's
        // session so a terminal hang-up never reaches it.
        unsafe {
            command.pre_exec(|| {
                libc_setsid();
                Ok(())
            });
        }
    }
    command
        .spawn()
        .map_err(|err| LaunchError::Spawn(format!("{}: {err}", argv[0])))
}

/// `setsid(2)` without a libc dependency: the syscall through `rustix`
/// is what `duet-vfs` uses elsewhere; here the process-group call above
/// already covers signals, and `setsid` is best-effort on top of it.
fn libc_setsid() {
    let _ = rustix::process::setsid();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_for_entries_commands_and_executables() {
        let entry = DesktopEntry {
            id: "e.desktop".into(),
            path: "/e.desktop".into(),
            name: "Editor".into(),
            exec: "editor --open %f".into(),
            try_exec: None,
            working_dir: Some("/work".into()),
            terminal: true,
            icon: None,
            no_display: false,
            mime_types: Vec::new(),
        };
        let plan = LaunchPlan::for_entry(&entry, &[PathBuf::from("/tmp/x.txt")]).unwrap();
        assert_eq!(plan.argv, ["editor", "--open", "/tmp/x.txt"]);
        assert_eq!(plan.cwd, Some(PathBuf::from("/work")));
        assert!(plan.terminal);
        assert_eq!(plan.label, "Editor");

        let plan =
            LaunchPlan::for_command("zed %f --wait", &[PathBuf::from("/tmp/x.txt")]).unwrap();
        assert_eq!(plan.argv, ["zed", "/tmp/x.txt", "--wait"]);
        assert_eq!(plan.cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(plan.label, "zed");
        assert!(LaunchPlan::for_command("   ", &[]).is_none());

        let plan = LaunchPlan::for_executable(Path::new("/tmp/run.sh"), true);
        assert_eq!(plan.argv, ["/tmp/run.sh"]);
        assert_eq!(plan.cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(plan.label, "run.sh");
        assert!(plan.terminal);
        assert!(!plan.program_available(), "not a real file");
        assert!(LaunchPlan::for_executable(Path::new("/bin/sh"), false).program_available());
    }

    #[test]
    fn terminal_wrapping_holds_the_window_open() {
        let term = TerminalLauncher {
            prefix: vec!["kgx".into(), "--".into()],
        };
        assert_eq!(
            term.wrap(&["ls".to_string(), "-l".to_string()], false),
            ["kgx", "--", "ls", "-l"]
        );
        let held = term.wrap(&["ls".to_string()], true);
        assert_eq!(&held[..4], &["kgx", "--", "sh", "-c"]);
        assert!(held[4].contains("Press Enter"));
        assert_eq!(held[5], "ls");
        assert_eq!(
            TerminalLauncher::for_program("xterm").map(|t| t.prefix.len()),
            program_exists("xterm").then_some(2)
        );
        assert!(TerminalLauncher::for_program("not-a-terminal-xyz").is_none());
    }

    #[test]
    fn spawn_runs_detached_and_reports_missing_programs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let plan = LaunchPlan {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("echo ok > '{}'", marker.display()),
            ],
            cwd: Some(dir.path().to_path_buf()),
            terminal: false,
            label: "sh".into(),
        };
        let mut child = spawn(&plan).expect("sh spawns");
        assert!(child.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "ok");

        let missing = LaunchPlan {
            argv: vec!["definitely-not-installed-xyz".into()],
            cwd: None,
            terminal: false,
            label: "x".into(),
        };
        assert_eq!(
            spawn(&missing).err(),
            Some(LaunchError::ProgramNotFound(
                "definitely-not-installed-xyz".into()
            ))
        );
        assert!(
            LaunchError::NoTerminal.to_string().contains("$TERMINAL"),
            "the message tells the user what to set"
        );
    }
}
