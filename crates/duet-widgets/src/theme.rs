// SPDX-License-Identifier: MIT
//! Theme façade over `gpui_component::theme` (R-G7), plus Duet's own
//! documented token set (T-4.1.5, `docs/config-schema.md` §4).
//!
//! `ActiveTheme`/`Theme`/`ThemeMode` are `gpui-component`'s own theme
//! plumbing (light/dark mode switching, the ~90-field `ThemeColor` widgets
//! read from); re-exported here per the same "no crate but this one writes
//! `use gpui_component::...`" rule the rest of this façade enforces.
//! [`TokenPalette`] is different: it is Duet's *own* ~48-token
//! color/spacing palette, specified in full by `docs/config-schema.md` §4
//! (not invented here), loaded from `themes/*.toml` via
//! `duet-config::theme::ThemeTokensDocument` and applied on top of
//! `gpui-component`'s theme so the two stay visually coherent. See
//! [`TokenPalette::install`] for how the two meet.

use std::collections::HashMap;

use gpui::{App, Global, Hsla, Pixels, Rgba, px};

pub use gpui_component::{ActiveTheme, Theme, ThemeMode};

/// One color token's canonical name (`docs/config-schema.md` §4's `[color]`
/// table) paired with a mutable accessor into [`ColorTokens`]. Used to drive
/// [`ColorTokens::apply_overrides`] and [`ColorTokens::built_in`] from one
/// list instead of duplicating the 32 names across several match arms.
type ColorField = (&'static str, fn(&mut ColorTokens) -> &mut Hsla);
type SyntaxField = (&'static str, fn(&mut SyntaxTokens) -> &mut Hsla);
type SpacingField = (&'static str, fn(&mut SpacingTokens) -> &mut Pixels);

/// Duet's `[color]` token set, verbatim per `docs/config-schema.md` §4 (32
/// tokens). Every field name matches that document's token name exactly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorTokens {
    pub panel_bg_active: Hsla,
    pub panel_bg_inactive: Hsla,
    pub panel_fg_active: Hsla,
    pub panel_fg_inactive: Hsla,
    pub cursor_bg: Hsla,
    pub cursor_fg: Hsla,
    pub selection_bg: Hsla,
    pub selection_fg: Hsla,
    pub header_bg: Hsla,
    pub header_fg: Hsla,
    pub statusbar_bg: Hsla,
    pub statusbar_fg: Hsla,
    pub border_default: Hsla,
    pub border_focus: Hsla,
    pub scrollbar_thumb: Hsla,
    pub scrollbar_track: Hsla,
    pub accent: Hsla,
    pub error: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
    pub info: Hsla,
    pub link: Hsla,
    pub dialog_bg: Hsla,
    pub dialog_fg: Hsla,
    pub tooltip_bg: Hsla,
    pub tooltip_fg: Hsla,
    pub tab_active_bg: Hsla,
    pub tab_inactive_bg: Hsla,
    pub progress_bar: Hsla,
    pub progress_track: Hsla,
    pub disabled_fg: Hsla,
    pub icon_folder: Hsla,
    pub icon_file: Hsla,
}

/// Duet's `[syntax]` token set, verbatim per `docs/config-schema.md` §4 (8
/// tokens).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyntaxTokens {
    pub keyword: Hsla,
    pub string: Hsla,
    pub comment: Hsla,
    pub number: Hsla,
    pub function: Hsla,
    pub r#type: Hsla,
    pub diff_add: Hsla,
    pub diff_remove: Hsla,
}

/// Duet's `[spacing]` token set, verbatim per `docs/config-schema.md` §4 (8
/// tokens, integer px at 1x scale -- HiDPI scaling is applied at render
/// time by GPUI, not baked into the theme, per that section's note).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpacingTokens {
    pub xs: Pixels,
    pub sm: Pixels,
    pub md: Pixels,
    pub lg: Pixels,
    pub xl: Pixels,
    pub radius_sm: Pixels,
    pub radius_md: Pixels,
    pub radius_lg: Pixels,
}

/// The full Duet token palette: `docs/config-schema.md` §4's `[color]` +
/// `[syntax]` + `[spacing]` tables, resolved to real `gpui` values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenPalette {
    pub color: ColorTokens,
    pub syntax: SyntaxTokens,
    pub spacing: SpacingTokens,
}

impl Global for TokenPalette {}

/// `#rrggbb`/`#rrggbbaa` -> [`Hsla`]. Returns `None` (rather than a hard
/// error) on a malformed hex string so one bad token in a hand-edited
/// theme file degrades to the built-in default for that token instead of
/// failing the whole load -- consistent with `duet-config::watch`'s own
/// "a malformed reload does not tear down the running config" posture.
fn parse_hex(hex: &str) -> Option<Hsla> {
    Rgba::try_from(hex).ok().map(Hsla::from)
}

macro_rules! color_fields {
    ($($name:literal => $field:ident),+ $(,)?) => {
        &[$(($name, (|t: &mut ColorTokens| &mut t.$field) as fn(&mut ColorTokens) -> &mut Hsla)),+]
    };
}

macro_rules! syntax_fields {
    ($($name:literal => $field:ident),+ $(,)?) => {
        &[$(($name, (|t: &mut SyntaxTokens| &mut t.$field) as fn(&mut SyntaxTokens) -> &mut Hsla)),+]
    };
}

macro_rules! spacing_fields {
    ($($name:literal => $field:ident),+ $(,)?) => {
        &[$(($name, (|t: &mut SpacingTokens| &mut t.$field) as fn(&mut SpacingTokens) -> &mut Pixels)),+]
    };
}

/// `(token name, accessor)` for every [`ColorTokens`] field, in
/// `docs/config-schema.md` §4's own order.
const COLOR_FIELDS: &[ColorField] = color_fields! {
    "panel_bg_active" => panel_bg_active,
    "panel_bg_inactive" => panel_bg_inactive,
    "panel_fg_active" => panel_fg_active,
    "panel_fg_inactive" => panel_fg_inactive,
    "cursor_bg" => cursor_bg,
    "cursor_fg" => cursor_fg,
    "selection_bg" => selection_bg,
    "selection_fg" => selection_fg,
    "header_bg" => header_bg,
    "header_fg" => header_fg,
    "statusbar_bg" => statusbar_bg,
    "statusbar_fg" => statusbar_fg,
    "border_default" => border_default,
    "border_focus" => border_focus,
    "scrollbar_thumb" => scrollbar_thumb,
    "scrollbar_track" => scrollbar_track,
    "accent" => accent,
    "error" => error,
    "warning" => warning,
    "success" => success,
    "info" => info,
    "link" => link,
    "dialog_bg" => dialog_bg,
    "dialog_fg" => dialog_fg,
    "tooltip_bg" => tooltip_bg,
    "tooltip_fg" => tooltip_fg,
    "tab_active_bg" => tab_active_bg,
    "tab_inactive_bg" => tab_inactive_bg,
    "progress_bar" => progress_bar,
    "progress_track" => progress_track,
    "disabled_fg" => disabled_fg,
    "icon_folder" => icon_folder,
    "icon_file" => icon_file,
};

/// `(token name, accessor)` for every [`SyntaxTokens`] field.
const SYNTAX_FIELDS: &[SyntaxField] = syntax_fields! {
    "keyword" => keyword,
    "string" => string,
    "comment" => comment,
    "number" => number,
    "function" => function,
    "type" => r#type,
    "diff_add" => diff_add,
    "diff_remove" => diff_remove,
};

/// `(token name, accessor)` for every [`SpacingTokens`] field.
const SPACING_FIELDS: &[SpacingField] = spacing_fields! {
    "xs" => xs,
    "sm" => sm,
    "md" => md,
    "lg" => lg,
    "xl" => xl,
    "radius_sm" => radius_sm,
    "radius_md" => radius_md,
    "radius_lg" => radius_lg,
};

/// Built-in dark palette: `docs/config-schema.md` §4's worked example,
/// verbatim (Catppuccin Mocha's published hex values -- that document's
/// example already *is* that palette; kept byte-for-byte identical rather
/// than re-deriving it).
fn built_in_dark_hex() -> (
    HashMap<&'static str, &'static str>,
    HashMap<&'static str, &'static str>,
) {
    let color = HashMap::from([
        ("panel_bg_active", "#1e1e2e"),
        ("panel_bg_inactive", "#181825"),
        ("panel_fg_active", "#cdd6f4"),
        ("panel_fg_inactive", "#a6adc8"),
        ("cursor_bg", "#89b4fa"),
        ("cursor_fg", "#1e1e2e"),
        ("selection_bg", "#45475a"),
        ("selection_fg", "#cdd6f4"),
        ("header_bg", "#181825"),
        ("header_fg", "#bac2de"),
        ("statusbar_bg", "#11111b"),
        ("statusbar_fg", "#cdd6f4"),
        ("border_default", "#313244"),
        ("border_focus", "#89b4fa"),
        ("scrollbar_thumb", "#45475a"),
        ("scrollbar_track", "#181825"),
        ("accent", "#89b4fa"),
        ("error", "#f38ba8"),
        ("warning", "#f9e2af"),
        ("success", "#a6e3a1"),
        ("info", "#89dceb"),
        ("link", "#74c7ec"),
        ("dialog_bg", "#1e1e2e"),
        ("dialog_fg", "#cdd6f4"),
        ("tooltip_bg", "#11111b"),
        ("tooltip_fg", "#cdd6f4"),
        ("tab_active_bg", "#313244"),
        ("tab_inactive_bg", "#181825"),
        ("progress_bar", "#89b4fa"),
        ("progress_track", "#313244"),
        ("disabled_fg", "#6c7086"),
        ("icon_folder", "#89b4fa"),
        ("icon_file", "#bac2de"),
    ]);
    let syntax = HashMap::from([
        ("keyword", "#cba6f7"),
        ("string", "#a6e3a1"),
        ("comment", "#6c7086"),
        ("number", "#fab387"),
        ("function", "#89b4fa"),
        ("type", "#f9e2af"),
        ("diff_add", "#a6e3a1"),
        ("diff_remove", "#f38ba8"),
    ]);
    (color, syntax)
}

/// Built-in light palette: Catppuccin Latte, the official light counterpart
/// to Mocha (same open, MIT-licensed palette family the dark default
/// already uses), mapped token-for-token onto the same semantic roles as
/// [`built_in_dark_hex`] so switching modes changes *lightness*, not which
/// role each hue plays. `docs/config-schema.md` §4 only worked-examples the
/// dark variant; this is Duet's own choice of light complement, kept
/// internally consistent with it rather than an unrelated palette.
fn built_in_light_hex() -> (
    HashMap<&'static str, &'static str>,
    HashMap<&'static str, &'static str>,
) {
    let color = HashMap::from([
        ("panel_bg_active", "#eff1f5"),
        ("panel_bg_inactive", "#e6e9ef"),
        ("panel_fg_active", "#4c4f69"),
        ("panel_fg_inactive", "#5c5f77"),
        ("cursor_bg", "#1e66f5"),
        ("cursor_fg", "#eff1f5"),
        ("selection_bg", "#bcc0cc"),
        ("selection_fg", "#4c4f69"),
        ("header_bg", "#e6e9ef"),
        ("header_fg", "#6c6f85"),
        ("statusbar_bg", "#dce0e8"),
        ("statusbar_fg", "#4c4f69"),
        ("border_default", "#ccd0da"),
        ("border_focus", "#1e66f5"),
        ("scrollbar_thumb", "#bcc0cc"),
        ("scrollbar_track", "#e6e9ef"),
        ("accent", "#1e66f5"),
        ("error", "#d20f39"),
        ("warning", "#df8e1d"),
        ("success", "#40a02b"),
        ("info", "#04a5e5"),
        ("link", "#209fb5"),
        ("dialog_bg", "#eff1f5"),
        ("dialog_fg", "#4c4f69"),
        ("tooltip_bg", "#dce0e8"),
        ("tooltip_fg", "#4c4f69"),
        ("tab_active_bg", "#ccd0da"),
        ("tab_inactive_bg", "#e6e9ef"),
        ("progress_bar", "#1e66f5"),
        ("progress_track", "#ccd0da"),
        ("disabled_fg", "#9ca0b0"),
        ("icon_folder", "#1e66f5"),
        ("icon_file", "#6c6f85"),
    ]);
    let syntax = HashMap::from([
        ("keyword", "#8839ef"),
        ("string", "#40a02b"),
        ("comment", "#9ca0b0"),
        ("number", "#fe640b"),
        ("function", "#1e66f5"),
        ("type", "#df8e1d"),
        ("diff_add", "#40a02b"),
        ("diff_remove", "#d20f39"),
    ]);
    (color, syntax)
}

/// Spacing is density-independent of the color scheme -- both built-in
/// variants share `docs/config-schema.md` §4's spacing scale.
fn built_in_spacing() -> HashMap<&'static str, f64> {
    HashMap::from([
        ("xs", 2.0),
        ("sm", 4.0),
        ("md", 8.0),
        ("lg", 16.0),
        ("xl", 24.0),
        ("radius_sm", 2.0),
        ("radius_md", 4.0),
        ("radius_lg", 8.0),
    ])
}

impl ColorTokens {
    fn from_hex_map(defaults: &HashMap<&'static str, &'static str>) -> Self {
        let mut tokens = Self::zeroed();
        for &(name, accessor) in COLOR_FIELDS {
            if let Some(hex) = defaults.get(name).and_then(|h| parse_hex(h)) {
                *accessor(&mut tokens) = hex;
            }
        }
        tokens
    }

    fn zeroed() -> Self {
        let black = Hsla {
            h: 0.,
            s: 0.,
            l: 0.,
            a: 1.,
        };
        Self {
            panel_bg_active: black,
            panel_bg_inactive: black,
            panel_fg_active: black,
            panel_fg_inactive: black,
            cursor_bg: black,
            cursor_fg: black,
            selection_bg: black,
            selection_fg: black,
            header_bg: black,
            header_fg: black,
            statusbar_bg: black,
            statusbar_fg: black,
            border_default: black,
            border_focus: black,
            scrollbar_thumb: black,
            scrollbar_track: black,
            accent: black,
            error: black,
            warning: black,
            success: black,
            info: black,
            link: black,
            dialog_bg: black,
            dialog_fg: black,
            tooltip_bg: black,
            tooltip_fg: black,
            tab_active_bg: black,
            tab_inactive_bg: black,
            progress_bar: black,
            progress_track: black,
            disabled_fg: black,
            icon_folder: black,
            icon_file: black,
        }
    }
}

impl SyntaxTokens {
    fn from_hex_map(defaults: &HashMap<&'static str, &'static str>) -> Self {
        let black = Hsla {
            h: 0.,
            s: 0.,
            l: 0.,
            a: 1.,
        };
        let mut tokens = Self {
            keyword: black,
            string: black,
            comment: black,
            number: black,
            function: black,
            r#type: black,
            diff_add: black,
            diff_remove: black,
        };
        for &(name, accessor) in SYNTAX_FIELDS {
            if let Some(hex) = defaults.get(name).and_then(|h| parse_hex(h)) {
                *accessor(&mut tokens) = hex;
            }
        }
        tokens
    }
}

impl SpacingTokens {
    fn from_px_map(defaults: &HashMap<&'static str, f64>) -> Self {
        let mut tokens = Self {
            xs: px(0.),
            sm: px(0.),
            md: px(0.),
            lg: px(0.),
            xl: px(0.),
            radius_sm: px(0.),
            radius_md: px(0.),
            radius_lg: px(0.),
        };
        for &(name, accessor) in SPACING_FIELDS {
            if let Some(&value) = defaults.get(name) {
                *accessor(&mut tokens) = px(value as f32);
            }
        }
        tokens
    }
}

impl TokenPalette {
    /// The built-in default palette for `mode` -- Catppuccin Mocha (dark,
    /// `docs/config-schema.md` §4's own worked example) or Catppuccin Latte
    /// (light, Duet's chosen complement). Used before any `themes/*.toml`
    /// override is applied.
    pub fn built_in(mode: ThemeMode) -> Self {
        let (color_hex, syntax_hex) = if mode.is_dark() {
            built_in_dark_hex()
        } else {
            built_in_light_hex()
        };
        let spacing_hex = built_in_spacing();
        Self {
            color: ColorTokens::from_hex_map(&color_hex),
            syntax: SyntaxTokens::from_hex_map(&syntax_hex),
            spacing: SpacingTokens::from_px_map(&spacing_hex),
        }
    }

    /// Overrides whichever tokens `color`/`syntax`/`spacing` specify (by
    /// the documented token name, matching a loaded
    /// `duet_config::theme::ThemeTokensDocument`'s three tables), leaving
    /// every other token at its current (built-in default) value --
    /// `docs/config-schema.md` §4's "custom theme file ... overriding the
    /// built-in default where the file specifies them". A malformed hex
    /// value for a given token is skipped (see [`parse_hex`]), not fatal to
    /// the rest of the load.
    pub fn apply_overrides(
        &mut self,
        color: &HashMap<String, String>,
        syntax: &HashMap<String, String>,
        spacing: &HashMap<String, f64>,
    ) {
        for &(name, accessor) in COLOR_FIELDS {
            if let Some(hsla) = color.get(name).and_then(|h| parse_hex(h)) {
                *accessor(&mut self.color) = hsla;
            }
        }
        for &(name, accessor) in SYNTAX_FIELDS {
            if let Some(hsla) = syntax.get(name).and_then(|h| parse_hex(h)) {
                *accessor(&mut self.syntax) = hsla;
            }
        }
        for &(name, accessor) in SPACING_FIELDS {
            if let Some(&value) = spacing.get(name) {
                *accessor(&mut self.spacing) = px(value as f32);
            }
        }
    }

    /// Installs `self` as the process-wide [`TokenPalette`] global (read
    /// back via [`TokenPalette::current`]) and maps a representative subset
    /// of its tokens onto `gpui-component`'s own [`Theme`]/`ThemeColor` so
    /// façade widgets (Input, Table, ...) stay visually coherent with
    /// Duet's palette rather than showing gpui-component's unrelated
    /// built-in colors alongside it. Not an exhaustive field-for-field
    /// mapping (`ThemeColor` has roughly 90 fields to this palette's ~40;
    /// only the roles Duet's current shell surfaces -- panel/status/border/
    /// accent/danger chrome -- are mapped), but every mapped field is a
    /// real, named correspondence, not a placeholder.
    pub fn install(self, cx: &mut App) {
        if cx.has_global::<Theme>() {
            let theme = Theme::global_mut(cx);
            theme.background = self.color.panel_bg_active;
            theme.foreground = self.color.panel_fg_active;
            theme.border = self.color.border_default;
            theme.ring = self.color.border_focus;
            theme.muted = self.color.panel_bg_inactive;
            theme.muted_foreground = self.color.panel_fg_inactive;
            theme.accent = self.color.accent;
            theme.primary = self.color.accent;
            theme.primary_foreground = self.color.panel_bg_active;
            theme.danger = self.color.error;
            theme.danger_foreground = self.color.panel_bg_active;
            theme.info = self.color.info;
            theme.link = self.color.link;
            theme.selection = self.color.selection_bg;
            theme.popover = self.color.dialog_bg;
            theme.popover_foreground = self.color.dialog_fg;
            theme.scrollbar_thumb = self.color.scrollbar_thumb;
            theme.scrollbar = self.color.scrollbar_track;
            theme.progress_bar = self.color.progress_bar;
            theme.input = self.color.border_default;
            theme.caret = self.color.cursor_bg;
            // T-4.3.8 UAT: `Table`'s own row-hover style (`table_hover`)
            // replaces a row's background outright -- it doesn't touch
            // text color, and it applies the same way to every row,
            // cursor or not. That rules out a light/bright hover color
            // (a first attempt using `info`, a light "sky" cyan, made
            // hovering any *ordinary* row -- by far the common case --
            // show `panel_fg_active`'s light text on a light background,
            // "almost indistinguishable" per UAT) as well as a dark one
            // close to the row's own base background (would fail the
            // *cursor* row's dark `cursor_fg` text the same way the
            // stock gray originally did). `tab_active_bg` is this
            // palette's own "elevated/currently-relevant surface" role --
            // a genuine middle tone in both built-in themes -- so it
            // holds up reasonably against light default-row text (the
            // common case) without being actively unreadable against the
            // cursor row's dark text on the rare occasion the two
            // coincide.
            theme.table_hover = self.color.tab_active_bg;
        }
        cx.set_global(self);
    }

    /// Reads the currently installed palette. Panics if [`Self::install`]
    /// was never called -- every window-open path installs one before
    /// building any view (see `duet-ui`'s `theme_controller` module), so a
    /// missing global here is a startup-ordering bug, not a runtime
    /// condition to recover from.
    pub fn current(cx: &App) -> &TokenPalette {
        cx.global::<TokenPalette>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_dark_matches_config_schema_worked_example() {
        let palette = TokenPalette::built_in(ThemeMode::Dark);
        // Round-trip a known token through the same hex parser used for
        // overrides, and confirm the built-in matches it exactly.
        assert_eq!(
            palette.color.accent,
            parse_hex("#89b4fa").expect("valid hex")
        );
        assert_eq!(palette.spacing.md, px(8.));
    }

    #[test]
    fn apply_overrides_replaces_only_named_tokens() {
        let mut palette = TokenPalette::built_in(ThemeMode::Dark);
        let original_border = palette.color.border_default;

        let mut color = HashMap::new();
        color.insert("accent".to_string(), "#ff00ff".to_string());
        palette.apply_overrides(&color, &HashMap::new(), &HashMap::new());

        assert_eq!(
            palette.color.accent,
            parse_hex("#ff00ff").expect("valid hex")
        );
        assert_eq!(
            palette.color.border_default, original_border,
            "un-overridden tokens must keep their built-in value"
        );
    }

    #[test]
    fn apply_overrides_ignores_malformed_hex_without_panicking() {
        let mut palette = TokenPalette::built_in(ThemeMode::Light);
        let original = palette.color.accent;

        let mut color = HashMap::new();
        color.insert("accent".to_string(), "not-a-color".to_string());
        palette.apply_overrides(&color, &HashMap::new(), &HashMap::new());

        assert_eq!(palette.color.accent, original);
    }

    #[test]
    fn dark_and_light_built_ins_differ() {
        let dark = TokenPalette::built_in(ThemeMode::Dark);
        let light = TokenPalette::built_in(ThemeMode::Light);
        assert_ne!(dark.color.panel_bg_active, light.color.panel_bg_active);
        assert_ne!(dark.color.panel_fg_active, light.color.panel_fg_active);
    }
}
