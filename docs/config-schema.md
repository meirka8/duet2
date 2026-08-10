# Duet configuration schema

Status: draft (T-1.6.1). Feeds T-3.3.1 (config loading: round-trip preservation, schema
versioning, migration runner, hot reload). Satisfies FR-CFG-01 (plain-text TOML under XDG
paths, hot-reloaded, settings UI edits the same files) and FR-CFG-04 (theming with a
documented token set).

## 0. Directory layout

Per `design.md` §10:

```
~/.config/duet/
    settings.toml          # everything except keys and themes
    keymap.toml             # user layer
    keymaps/tc.toml          # shipped bases (read-only, copied on customise)
    keymaps/mc.toml
    keymaps/modern.toml
    hotlist.toml             # bookmarks (own schema, out of scope for this doc)
    connections.toml         # remote profiles (no secrets — keyring refs only)
    themes/*.toml
    plugins/<id>/            # installed plugin bundles + per-plugin config
~/.local/state/duet/
    session.json             # panes, tabs, cwds, cursor positions, sort state (NOT config; excluded here)
    history/                 # command line, masks, search, paths
    jobs/*.journal            # operation journals (§9.3, FR-OPS-07)
~/.cache/duet/
    dirsize.db                # directory size cache
    icons/                     # rasterised icon cache
```

Everything below documents the four config-file schemas in scope for T-1.6.1:
`settings.toml`, `keymap.toml` (plus its base files), `connections.toml`, and the theme
token list used by `themes/*.toml`. Per-tab UI state (column widths, sort column actually
in effect, cursor position) lives in `session.json`, not in `settings.toml` — that file is
runtime state, not user-authored configuration, so it is out of scope here.

## Hot reload (FR-CFG-01)

All four file kinds are watched via `notify` (inotify) from the I/O runtime, never the UI
thread (§8.2 — the UI thread performs no syscalls, full stop). A ~50 ms debounce absorbs
editor save patterns (write-then-rename, multiple writes), then the file is re-parsed off
the UI thread and diffed against the in-memory model; only the changed leaves are applied,
delivered to the UI as an event-stream diff like any other core→UI update. Total edit-to-
effect latency budget is 200 ms (T-3.3.1 AC), comfortably inside the debounce + parse +
diff pipeline. A malformed file on reload does **not** tear down the running config: the
last-known-good in-memory value is kept, and a non-blocking diagnostic (file, line, column)
is surfaced. Settings-UI writes (T-9.1.17) go through the same path — the UI never mutates
its own in-memory model directly, it writes the file and waits for the watched reload, which
keeps "edited via UI" and "edited externally" a single code path.

## Schema versioning and migration

Every file carries a top-level `schema_version` integer (themes: same field, see §4).
Migrations are forward-only, keyed on `(file_kind, schema_version)`, and run synchronously
on load before the file is handed to its consumer. Before a migration writes back the
upgraded file, it writes a timestamped backup alongside the original
(`settings.toml.v0.bak-<unix_ts>`), matching T-3.3.1's acceptance criterion. Parsing uses a
round-trip-preserving TOML representation (`toml_edit`, not `toml`/serde-only) so that keys
Duet doesn't recognize — future keys from a newer version, or third-party additions — survive
being rewritten by an older or same-version Duet, along with comments and formatting where
feasible. `schema_version = 0` is reserved for files predating this field (treated as
implicitly version 0 and migrated up on first load by the version that introduces this doc's
schema, which is version 1).

| File | Current `schema_version` |
|---|---|
| `settings.toml` | 1 |
| `keymap.toml` / `keymaps/*.toml` | 1 |
| `connections.toml` | 1 |
| `themes/*.toml` | 1 |

---

## 1. `settings.toml`

Everything except keybindings and theme color/spacing values. Grouped by the panel/dialog
that would edit it in the future settings UI (T-9.1.17).

```toml
schema_version = 1

[general]
locale                          = "system"     # "system" | BCP-47 tag, e.g. "de-DE"
fallback_locale                 = "en-US"
startup_behavior                = "restore_session" # restore_session | open_home | open_last_cwd | open_specified
confirm_quit_with_running_jobs  = true
single_instance                 = true

[panels]
sort_directories_first  = true
natural_sort             = true
case_sensitive_sort      = false
show_hidden              = false
default_view             = "full"          # full | brief | thumbnails | tree
default_sort_column      = "name"          # name | ext | size | date | attrs
default_sort_order       = "ascending"     # ascending | descending
remember_view_per_tab    = true

[selection]
mouse_mode                        = "windows"  # windows | norton | none
restore_selection_after_operation = true

[navigation]
quick_search_mode            = "jump"   # jump | filter
quick_search_idle_timeout_ms = 1000     # 200-5000; resets the typed buffer after this much idle time
history_size                  = 100      # per-tab back/forward history depth, 10-1000
branch_view_show_hidden       = false

[operations]
verify_after_copy               = false
preserve_xattrs                 = true
preserve_acls                   = true
preserve_timestamps             = true
preserve_ownership_if_privileged = true
concurrency                     = "auto" # "auto" | 1-32; worker count, see design.md §8.2
delete_default                  = "trash"  # trash | permanent
confirm_delete                  = "always" # always | non_empty_dirs | never
default_conflict_policy         = "ask"    # ask | skip | overwrite | overwrite_if_older | overwrite_if_different_size | rename | auto_rename | abort
journal_retention_days          = 7        # 0 = keep forever; completed-job journals older than this are pruned

[trash]
enabled                        = true
use_top_level_on_other_mounts  = true
confirm_empty                  = true

[appearance]
theme_follow_system = true
theme_light          = "duet-light"
theme_dark           = "duet-dark"
theme                = "duet-dark"   # used only when theme_follow_system = false
font                 = "system-ui"
font_size            = 13            # 8-32
row_height           = "compact"     # compact | comfortable | spacious
icon_theme           = "system"      # "system" | an installed XDG icon theme name
show_icons           = true

[terminal]
shell                     = "$SHELL"
embedded_terminal_enabled = false

[clipboard]
cut_marker_convention = "auto" # auto | gnome | kde

[logging]
log_level   = "info"  # error | warn | info | debug | trace
log_to_file = true

[plugins]
enabled   = true
directory = "~/.config/duet/plugins"
```

Validated with Python's `tomllib` — parses cleanly.

### Key reference

| Key | Type | Default | Range / values | Meaning |
|---|---|---|---|---|
| `schema_version` | int | `1` | ≥ 0 | Migration marker for this file. |
| `general.locale` | string | `"system"` | `"system"` \| BCP-47 tag | UI language; `"system"` reads `$LANG`/portal locale. FR-CFG-10. |
| `general.fallback_locale` | string | `"en-US"` | any BCP-47 tag with a bundled Fluent catalogue | Used when the resolved locale has no translation for a string. |
| `general.startup_behavior` | enum | `"restore_session"` | `restore_session` \| `open_home` \| `open_last_cwd` \| `open_specified` | What each panel shows on launch absent CLI args (FR-CFG-09 args always win). |
| `general.confirm_quit_with_running_jobs` | bool | `true` | — | Prompt before quitting while the operation queue is non-empty. |
| `general.single_instance` | bool | `true` | — | Second launch forwards its CLI to the running instance (FR-CFG-08); `--new-instance` overrides. |
| `panels.sort_directories_first` | bool | `true` | — | FR-NAV-06 directories-first policy. |
| `panels.natural_sort` | bool | `true` | — | Natural (version) sort for numeric runs in names. |
| `panels.case_sensitive_sort` | bool | `false` | — | Case sensitivity of the name comparator; collation is always locale-aware. |
| `panels.show_hidden` | bool | `false` | — | Show dotfiles / hidden-attribute entries by default. |
| `panels.default_view` | enum | `"full"` | `full` \| `brief` \| `thumbnails` \| `tree` | FR-NAV-04 column mode for newly opened tabs. |
| `panels.default_sort_column` | enum | `"name"` | `name` \| `ext` \| `size` \| `date` \| `attrs` | Initial sort key for newly opened tabs. |
| `panels.default_sort_order` | enum | `"ascending"` | `ascending` \| `descending` | Initial sort direction. |
| `panels.remember_view_per_tab` | bool | `true` | — | Whether view/sort changes persist per tab in `session.json` or reset to these defaults each launch. |
| `selection.mouse_mode` | enum | `"windows"` | `windows` \| `norton` \| `none` | FR-SEL-06: left-click-select vs. right-click-select vs. mouse never changes selection. |
| `selection.restore_selection_after_operation` | bool | `true` | — | FR-SEL-04. |
| `navigation.quick_search_mode` | enum | `"jump"` | `jump` \| `filter` | FR-NAV-07: typed letters jump to a match, or (modifier-prefixed by default, this key changes the unprefixed default) filter the panel. |
| `navigation.quick_search_idle_timeout_ms` | int | `1000` | 200–5000 | Idle time after the last keystroke before the typed quick-search buffer resets. |
| `navigation.history_size` | int | `100` | 10–1000 | Per-tab back/forward history depth (FR-NAV-08). |
| `navigation.branch_view_show_hidden` | bool | `false` | — | Hidden-file visibility specifically inside branch view (FR-NAV-10), independent of `panels.show_hidden`. |
| `operations.verify_after_copy` | bool | `false` | — | Global default for FR-OPS-08 post-copy checksum verification; overridable per job. |
| `operations.preserve_xattrs` | bool | `true` | — | FR-OPS-05. |
| `operations.preserve_acls` | bool | `true` | — | FR-OPS-05. |
| `operations.preserve_timestamps` | bool | `true` | — | mtime/atime preserved via `utimensat` after content+metadata (§9.3 ordering). |
| `operations.preserve_ownership_if_privileged` | bool | `true` | — | Ownership is only ever preserved when running privileged (§9.3); this toggles that behavior off even then. |
| `operations.concurrency` | string or int | `"auto"` | `"auto"` \| 1–32 | Operation-worker pool size; `"auto"` = one worker per rotational device, up to four for NVMe (§8.2). |
| `operations.delete_default` | enum | `"trash"` | `trash` \| `permanent` | Default target of the delete command. |
| `operations.confirm_delete` | enum | `"always"` | `always` \| `non_empty_dirs` \| `never` | Confirmation prompt policy for deletes. |
| `operations.default_conflict_policy` | enum | `"ask"` | `ask` \| `skip` \| `overwrite` \| `overwrite_if_older` \| `overwrite_if_different_size` \| `rename` \| `auto_rename` \| `abort` | Job-level default before any interactive "apply to all" answer (FR-OPS-04). |
| `operations.journal_retention_days` | int | `7` | 0–3650 (0 = forever) | Age after which completed job journals under `~/.local/state/duet/jobs/` are pruned. |
| `trash.enabled` | bool | `true` | — | Master switch for freedesktop trash (FR-CFG-07); when `false`, `delete_default` cannot be `trash`. |
| `trash.use_top_level_on_other_mounts` | bool | `true` | — | Use `$topdir/.Trash-$uid` for deletes on non-home filesystems, per spec. |
| `trash.confirm_empty` | bool | `true` | — | Confirm before emptying the trash. |
| `appearance.theme_follow_system` | bool | `true` | — | FR-CFG-04: follow the desktop dark/light preference; when `true`, `theme_light`/`theme_dark` are used and `theme` is ignored. |
| `appearance.theme_light` | string | `"duet-light"` | name of a file under `themes/` (without `.toml`) | Theme used for light mode when following the system. |
| `appearance.theme_dark` | string | `"duet-dark"` | name of a file under `themes/` | Theme used for dark mode when following the system. |
| `appearance.theme` | string | `"duet-dark"` | name of a file under `themes/` | Theme used when `theme_follow_system = false`. |
| `appearance.font` | string | `"system-ui"` | `"system-ui"` \| a fontconfig family name | UI font; `"system-ui"` reads fontconfig (R-G5). |
| `appearance.font_size` | int | `13` | 8–32 | Base UI font size in points. |
| `appearance.row_height` | enum | `"compact"` | `compact` \| `comfortable` \| `spacious` | Table row density. |
| `appearance.icon_theme` | string | `"system"` | `"system"` \| an installed XDG icon theme name | Icon theme resolution root (R-G5). |
| `appearance.show_icons` | bool | `true` | — | Disable to render text-only rows (accessibility / low-end GPU escape hatch). |
| `terminal.shell` | string | `"$SHELL"` | any executable path or env-var reference | Shell used by the embedded command line and terminal panel. |
| `terminal.embedded_terminal_enabled` | bool | `false` | — | FR-TOOL-07 toggle for the embedded terminal panel. |
| `clipboard.cut_marker_convention` | enum | `"auto"` | `auto` \| `gnome` \| `kde` | Which cut-marker MIME convention to emit/read for FR-CFG-05 interop; `auto` emits both and reads either. |
| `logging.log_level` | enum | `"info"` | `error` \| `warn` \| `info` \| `debug` \| `trace` | Default `tracing` filter; overridable by `--log-level`/`DUET_LOG`. |
| `logging.log_to_file` | bool | `true` | — | Persist the ring buffer / session log under `~/.local/state/duet/`. |
| `plugins.enabled` | bool | `true` | — | Master switch for the plugin host (FR-PLUG-\*). |
| `plugins.directory` | string | `"~/.config/duet/plugins"` | any path | Override for where installed plugin bundles are read from. |

---

## 2. `keymap.toml` and base keymaps

FR-CFG-02: the shipped default is TC-faithful; `mc`/`modern` bases are selectable; the user
file layers on top and can unbind. `design.md` §9.4 gives the sketch this formalizes.

**Layering model.** At load, the resolver reads exactly one base file —
`keymaps/{tc,mc,modern}.toml`, selected by `keymap.toml`'s own `base` key — as the
read-only starting binding set, then applies `keymap.toml` (the user layer) on top, in
file order:

1. Each `[[binding]]` in the user file is appended to the resolved set. If it collides with
   an existing `(context, key)` pair from the base (or an earlier user binding), the later
   one wins and the earlier one is reported as shadowed — a load-time diagnostic surfaces
   this rather than failing silently (§9.4 "conflict detection").
2. Each `[[unbind]]` removes a binding matching `context` and `key` (and `command` if given,
   for the case where disambiguating which of several same-key bindings to drop matters).
   Unbinding a binding that doesn't exist is a no-op diagnostic, not an error.
3. Base files themselves are never edited in place; "customise" in the settings UI means
   copying the relevant base binding into `keymap.toml` as an explicit override (design.md
   §10 directory comment: "shipped bases, read-only, copied on customise").

Base files (`tc.toml`, `mc.toml`, `modern.toml`) use the identical `[[binding]]` schema with
no `base` key of their own (a base cannot itself layer on another base).

```toml
schema_version = 1
base           = "tc"   # tc | mc | modern | none - the shipped base file this user layer is applied on top of

[[binding]]
context = "panel"
key     = "f5"
command = "ops.copy"

[[binding]]
context = "panel && selection.nonempty"
key     = "shift-f6"
command = "ops.rename_in_place"

[[binding]]
context = "panel"
key     = "ctrl-k ctrl-b"     # chord: ctrl-k then ctrl-b
command = "panel.branch_view"

[[binding]]
context = "panel"
key     = "alt-shift-i"
command = "sel.by_mask"
args    = { mask = "*.rs" }

[[unbind]]
context = "panel"
key     = "ctrl-w"            # user removes the inherited tab.close binding, e.g. to free it for the WM
```

Validated with Python's `tomllib` — parses cleanly.

### Key reference

| Key | Type | Default | Range / values | Meaning |
|---|---|---|---|---|
| `schema_version` | int | `1` | ≥ 0 | Migration marker. |
| `base` | enum | `"tc"` | `tc` \| `mc` \| `modern` \| `none` | Which shipped base file this user layer sits on top of; `none` starts from an empty binding set. Only meaningful in the user's `keymap.toml`. |
| `binding.context` | string | — (required) | a boolean predicate over context terms, e.g. `panel`, `viewer`, `dialog`, `cmdline`, `selection.nonempty`, `entry.is_dir`, `vfs.scheme == 'file'`, combined with `&&` / `\|\|` / `!` | GPUI context-predicate scope this binding is active in (§9.4). |
| `binding.key` | string | — (required) | modifiers joined with `-` (`ctrl`, `alt`, `shift`, `super`), key name, chord steps separated by a space (e.g. `"ctrl-k ctrl-b"`) | The key or key-chord that triggers `command`. |
| `binding.command` | string | — (required) | a registered command id from the command catalogue (`docs/commands.md`, T-1.5.1) | The command dispatched on match. |
| `binding.args` | inline table | none | command-specific, per its `args_schema` | Optional fixed arguments passed to the command (e.g. a preset mask). |
| `unbind.context` | string | — (required) | same grammar as `binding.context` | Context to match for removal. |
| `unbind.key` | string | — (required) | same grammar as `binding.key` | Key/chord to remove. |
| `unbind.command` | string | none (matches any) | a registered command id | Restricts removal to a specific command bound at that `(context, key)`, for the rare case of multiple bindings sharing a key across overlapping contexts. |

---

## 3. `connections.toml`

FR-VFS-04 remote backend profiles. **No secrets live here** — passwords, private keys,
access keys, and SSH passphrases are stored in the system Secret Service keyring
(`keyring`/`secret-service` crate) and referenced by `keyring_ref` only, per `design.md`
§7.5/§9.10/§13 ("Secrets: Remote credentials in the Secret Service keyring; never in config
files"). A keyring-less system degrades to session-only credential prompts (T-7.1.2 AC);
`connections.toml` is still fully readable/writable in that mode, just without a resolvable
`keyring_ref`.

```toml
schema_version = 1

[[connection]]
id          = "home-nas"
name        = "Home NAS"
backend     = "sftp"      # sftp | ftp | ftps | webdav | webdavs | s3 | smb
host        = "nas.local"
port        = 22
username    = "meir"
remote_path = "/srv/data"
keyring_ref = "duet:connection:home-nas"
connect_timeout_ms = 10000

[connection.options]
known_host_fingerprint = "SHA256:abc123def456"  # TOFU-pinned on first connect, per §13

[[connection]]
id          = "s3-backup"
name        = "S3 Backup"
backend     = "s3"
keyring_ref = "duet:connection:s3-backup"
connect_timeout_ms = 15000

[connection.options]
bucket     = "my-backup-bucket"
region     = "us-east-1"
endpoint   = "https://s3.amazonaws.com"
path_style = false

[[connection]]
id          = "office-share"
name        = "Office Share"
backend     = "smb"
host        = "fileserver.corp.example"
port        = 445
username    = "meir.kanevskiy"
keyring_ref = "duet:connection:office-share"

[connection.options]
share  = "Public"
domain = "CORP"
```

Validated with Python's `tomllib` — parses cleanly, and `[connection.options]` correctly
attaches as a subtable of the immediately preceding `[[connection]]` array element (standard
TOML array-of-tables nesting), not a fourth top-level connection.

### Key reference

| Key | Type | Default | Range / values | Meaning |
|---|---|---|---|---|
| `schema_version` | int | `1` | ≥ 0 | Migration marker. |
| `connection.id` | string | — (required) | unique, `[a-z0-9-]+` recommended | Stable identifier; used as the `keyring_ref` namespace and in URIs like `duet://home-nas/path`. |
| `connection.name` | string | — (required) | any | Human-readable label shown in the connection manager and mount bar (FR-NAV-11). |
| `connection.backend` | enum | — (required) | `sftp` \| `ftp` \| `ftps` \| `webdav` \| `webdavs` \| `s3` \| `smb` | Which VFS remote backend handles this profile (FR-VFS-04). |
| `connection.host` | string | none | hostname or IP | Required for `sftp`/`ftp`/`ftps`/`webdav`/`webdavs`/`smb`; omitted for `s3` (endpoint carries it, see `options.endpoint`). |
| `connection.port` | int | backend-specific (`22` sftp, `21` ftp, `445` smb, `443` webdavs) | 1–65535 | TCP port; omitted to use the backend default. |
| `connection.username` | string | none | any | Login identity; password/key material is never here (see keyring note above). |
| `connection.remote_path` | string | `"/"` | an absolute remote path | Initial path opened when the connection is mounted. |
| `connection.keyring_ref` | string | none | any; convention `duet:connection:<id>` | Lookup key into the Secret Service keyring for this profile's credential(s). |
| `connection.connect_timeout_ms` | int | `10000` | 1000–120000 | Connection-establishment timeout before surfacing a `Retryable`/`Fatal` error per the §9.3 taxonomy. |
| `connection.options.*` | table | `{}` | backend-specific, see below | Backend-specific fields that don't generalize across all seven backends. |

**`options` by backend** (informative, not exhaustive — new backend-specific keys are
additive and preserved by round-trip parsing even if an older Duet doesn't recognize them):

| Backend | Typical `options` keys |
|---|---|
| `sftp` | `known_host_fingerprint` (string, TOFU pin), `identity_file` (path to a public-key file; the private key/passphrase is keyring-only) |
| `ftp` / `ftps` | `passive_mode` (bool, default `true`) |
| `webdav` / `webdavs` | `verify_tls` (bool, default `true`) |
| `s3` | `bucket` (string, required), `region` (string), `endpoint` (URL, for S3-compatible providers), `path_style` (bool, default `false`) |
| `smb` | `share` (string, required), `domain` (string) |

---

## 4. Theme token list (`themes/*.toml`)

FR-CFG-04's "documented token set". A theme file supplies one variant (`light` or `dark`);
`appearance.theme_light` / `appearance.theme_dark` in `settings.toml` select which two files
form the auto-following pair, or `appearance.theme` selects a single fixed one. Icon
resolution is a separate concern (`appearance.icon_theme`, resolved against the installed
XDG icon theme, not against this token set — R-G5).

```toml
schema_version = 1
name    = "Duet Dark"
variant = "dark"   # dark | light - which appearance.theme_light/theme_dark slot this fills

[color]
panel_bg_active    = "#1e1e2e"
panel_bg_inactive  = "#181825"
panel_fg_active    = "#cdd6f4"
panel_fg_inactive  = "#a6adc8"
cursor_bg          = "#89b4fa"
cursor_fg          = "#1e1e2e"
selection_bg       = "#45475a"
selection_fg       = "#cdd6f4"
header_bg          = "#181825"
header_fg          = "#bac2de"
statusbar_bg       = "#11111b"
statusbar_fg       = "#cdd6f4"
border_default     = "#313244"
border_focus       = "#89b4fa"
scrollbar_thumb    = "#45475a"
scrollbar_track    = "#181825"
accent             = "#89b4fa"
error              = "#f38ba8"
warning            = "#f9e2af"
success            = "#a6e3a1"
info               = "#89dceb"
link               = "#74c7ec"
dialog_bg          = "#1e1e2e"
dialog_fg          = "#cdd6f4"
tooltip_bg         = "#11111b"
tooltip_fg         = "#cdd6f4"
tab_active_bg      = "#313244"
tab_inactive_bg    = "#181825"
progress_bar       = "#89b4fa"
progress_track     = "#313244"
disabled_fg        = "#6c7086"
icon_folder        = "#89b4fa"
icon_file          = "#bac2de"

[syntax]
keyword     = "#cba6f7"
string      = "#a6e3a1"
comment     = "#6c7086"
number      = "#fab387"
function    = "#89b4fa"
type        = "#f9e2af"
diff_add    = "#a6e3a1"
diff_remove = "#f38ba8"

[spacing]
xs        = 2
sm        = 4
md        = 8
lg        = 16
xl        = 24
radius_sm = 2
radius_md = 4
radius_lg = 8
```

Validated with Python's `tomllib` — parses cleanly.

### Token reference

All `[color]` and `[syntax]` values are `#rrggbb` or `#rrggbbaa` hex strings; `[spacing]`
values are integer pixels at 1x scale (HiDPI scaling per §14.5 is applied at render time,
not baked into the theme).

| Token | Table | Meaning |
|---|---|---|
| `panel_bg_active` / `panel_bg_inactive` | color | File table background, active vs. inactive panel (FR-NAV-02 header treatment). |
| `panel_fg_active` / `panel_fg_inactive` | color | Default row text color, active vs. inactive panel. |
| `cursor_bg` / `cursor_fg` | color | The cursor row (FR-SEL-01 cursor, distinct from selection). |
| `selection_bg` / `selection_fg` | color | Multi-selected rows. |
| `header_bg` / `header_fg` | color | Column header row. |
| `statusbar_bg` / `statusbar_fg` | color | Footer selection-stats bar (FR-SEL-05) and function-key bar. |
| `border_default` / `border_focus` | color | Panel/splitter/dialog borders; `border_focus` for the focused pane's border. |
| `scrollbar_thumb` / `scrollbar_track` | color | Scrollbar chrome. |
| `accent` | color | Primary interactive accent (focus rings, active tab underline, progress default). |
| `error` / `warning` / `success` / `info` | color | Status/severity colors used across dialogs, the operation tray, and inline diagnostics. |
| `link` | color | Hyperlink-like text (e.g. paths in error messages). |
| `dialog_bg` / `dialog_fg` | color | Modal overlay background/text (§9.5 dialogs). |
| `tooltip_bg` / `tooltip_fg` | color | Tooltips and the pending-chord indicator (§9.4). |
| `tab_active_bg` / `tab_inactive_bg` | color | Tab strip (FR-NAV-03). |
| `progress_bar` / `progress_track` | color | Operation progress bars (FR-OPS-03). |
| `disabled_fg` | color | Disabled menu items, unavailable commands in the palette (FR-TOOL-11). |
| `icon_folder` / `icon_file` | color | Tint applied to monochrome fallback icons when no themed icon resolves. |
| `syntax.keyword` / `string` / `comment` / `number` / `function` / `type` | syntax | Viewer/editor syntax highlighting (§9.6), shared with the tree-sitter-backed editor. |
| `syntax.diff_add` / `diff_remove` | syntax | Directory-compare (FR-OPS-10) and any future diff view. |
| `spacing.xs` … `spacing.xl` | spacing | Spacing scale (px @1x) for padding/gaps across dialogs and the shell chrome. |
| `spacing.radius_sm` … `radius_lg` | spacing | Corner radii for cards, dialogs, and buttons. |

Row *height* itself (`compact`/`comfortable`/`spacious`) is a `settings.toml` concern
(`appearance.row_height`), not a theme token, since it's a density preference independent of
color identity; themes only supply the color/spacing palette rendered at whatever density is
selected.

---

## Open items for T-3.3.1

- Exact grammar/parser for `binding.context` predicates (boolean expression over context
  terms) is design.md §9.4's responsibility; this doc only fixes the TOML field shape.
- `hotlist.toml` (bookmarks) is named in the directory layout but out of scope for T-1.6.1
  per the task's explicit four-file list; it will need its own short schema note before
  T-4.3.5.
- The migration runner's exact backup naming/retention (this doc proposes
  `<file>.v<N>.bak-<unix_ts>`) should be confirmed against whatever T-3.3.1 implements.
