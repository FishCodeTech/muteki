use std::{collections::HashMap, sync::Arc, sync::LazyLock};

use gpui::{App, Global, Hsla, Rgba, SharedString, Window, WindowAppearance, px};
use gpui_base::{
    ColorTokens, PlotTheme, RadiusTokens, ResizableTheme, ScrollbarMode, ScrollbarStyles,
    ScrollbarTheme, SemanticThemeTokens, ThemeAppearance, TypographyTokens,
};
use serde::Deserialize;

pub use crate::highlight::HighlightTheme;

const TCODE_THEME: &str = include_str!("../../../themes/tcode.json");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThemeMode {
    #[default]
    Light,
    Dark,
}

impl ThemeMode {
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

impl From<WindowAppearance> for ThemeMode {
    fn from(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
        }
    }
}

/// The application-owned visual values used directly by tcode render code.
#[derive(Debug, Clone)]
pub struct Theme {
    pub accent: Hsla,
    pub background: Hsla,
    pub border: Hsla,
    pub danger: Hsla,
    pub danger_active: Hsla,
    pub danger_foreground: Hsla,
    pub font_family: SharedString,
    pub foreground: Hsla,
    pub highlight_theme: Arc<HighlightTheme>,
    pub info: Hsla,
    pub info_foreground: Hsla,
    pub input: Hsla,
    pub link: Hsla,
    pub list_active: Hsla,
    pub list_hover: Hsla,
    pub mode: ThemeMode,
    pub mono_font_family: SharedString,
    pub muted: Hsla,
    pub muted_foreground: Hsla,
    pub popover: Hsla,
    pub primary: Hsla,
    pub primary_foreground: Hsla,
    pub ring: Hsla,
    pub scrollbar: Hsla,
    pub scrollbar_thumb: Hsla,
    pub scrollbar_thumb_hover: Hsla,
    pub secondary: Hsla,
    pub secondary_active: Hsla,
    pub selection: Hsla,
    pub sidebar: Hsla,
    pub sidebar_accent: Hsla,
    pub sidebar_foreground: Hsla,
    pub success: Hsla,
    pub success_foreground: Hsla,
    pub tab_active: Hsla,
    pub theme_name: SharedString,
    pub tokens: SemanticThemeTokens,
    pub warning: Hsla,
    pub warning_foreground: Hsla,
}

impl Theme {
    /// Connection severity shared by the sidebar and attached machine row.
    pub(crate) fn connection_color(&self, state: &tcode_client::ConnectionState) -> Hsla {
        match state {
            tcode_client::ConnectionState::Connected { .. }
            | tcode_client::ConnectionState::Syncing { .. } => self.success,
            tcode_client::ConnectionState::Reconnecting { .. } => self.warning,
            tcode_client::ConnectionState::Offline { .. } => self.danger,
        }
    }

    /// The composer's fast-mode bolt when fast mode is on. Deliberately not a
    /// theme token: one electric amber that reads on both light and dark
    /// surfaces, so the state is recognisable regardless of the active theme.
    pub(crate) fn fast_mode_accent(&self) -> Hsla {
        gpui::rgb(0xF5A524).into()
    }

    /// The effort slider's fill and label at the highest reasoning level.
    /// Deliberately not a theme token, for the same reason as
    /// [`Self::fast_mode_accent`].
    pub(crate) fn max_effort_accent(&self) -> Hsla {
        gpui::rgb(0x924FF7).into()
    }

    pub fn theme_name(&self) -> &SharedString {
        &self.theme_name
    }
}

impl Global for Theme {}

pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    #[inline(always)]
    fn theme(&self) -> &Theme {
        self.try_global::<Theme>()
            .unwrap_or_else(|| &embedded_themes().light)
    }
}

#[derive(Clone)]
struct Themes {
    light: Theme,
    dark: Theme,
}

impl Global for Themes {}

fn embedded_themes() -> &'static Themes {
    static THEMES: LazyLock<Themes> =
        LazyLock::new(|| parse_theme_file(TCODE_THEME).expect("embedded theme should parse"));
    &THEMES
}

#[derive(Deserialize)]
struct ThemeFile {
    themes: Vec<ThemeConfig>,
}

#[derive(Deserialize)]
struct ThemeConfig {
    name: String,
    mode: String,
    #[serde(rename = "font.family")]
    font_family: String,
    #[serde(rename = "mono_font.family")]
    mono_font_family: String,
    radius: f32,
    #[serde(rename = "radius.lg")]
    radius_lg: f32,
    colors: HashMap<String, String>,
}

fn parse_theme_file(source: &str) -> Result<Themes, String> {
    let file: ThemeFile = serde_json::from_str(source).map_err(|error| error.to_string())?;
    let mut light = None;
    let mut dark = None;

    for config in file.themes {
        let theme = Theme::try_from(config)?;
        match theme.mode {
            ThemeMode::Light => light = Some(theme),
            ThemeMode::Dark => dark = Some(theme),
        }
    }

    Ok(Themes {
        light: light.ok_or_else(|| "theme file is missing a light theme".to_string())?,
        dark: dark.ok_or_else(|| "theme file is missing a dark theme".to_string())?,
    })
}

impl TryFrom<ThemeConfig> for Theme {
    type Error = String;

    fn try_from(config: ThemeConfig) -> Result<Self, Self::Error> {
        let mode = match config.mode.as_str() {
            "light" => ThemeMode::Light,
            "dark" => ThemeMode::Dark,
            mode => return Err(format!("unsupported theme mode {mode:?}")),
        };
        let color = |key: &str| -> Result<Hsla, String> {
            let value = config
                .colors
                .get(key)
                .ok_or_else(|| format!("theme {:?} is missing color {key:?}", config.name))?;
            Rgba::try_from(value.as_str())
                .map(Into::into)
                .map_err(|error| {
                    format!("invalid color {key:?} in theme {:?}: {error}", config.name)
                })
        };

        let background = color("background")?;
        let foreground = color("foreground")?;
        let primary = color("primary.background")?;
        let primary_foreground = color("primary.foreground")?;
        let secondary = color("secondary.background")?;
        let muted = color("muted.background")?;
        let muted_foreground = color("muted.foreground")?;
        let accent = color("accent.background")?;
        let border = color("border")?;
        let input = color("input.border")?;
        let ring = color("ring")?;
        let popover = color("popover.background")?;
        let danger = color("danger.background")?;
        let danger_foreground = color("danger.foreground")?;
        let tokens = SemanticThemeTokens {
            colors: ColorTokens {
                selection: primary.alpha(0.3),
                background,
                foreground,
                surface: popover,
                surface_foreground: foreground,
                primary,
                primary_foreground,
                secondary,
                secondary_foreground: foreground,
                muted,
                muted_foreground,
                accent,
                accent_foreground: foreground,
                destructive: danger,
                destructive_foreground: danger_foreground,
                border,
                input,
                ring,
            },
            // The scale gpui-kit derives from the same two theme-file keys.
            radius: RadiusTokens {
                sm: px(config.radius / 2.),
                md: px(config.radius),
                lg: px(config.radius_lg),
                xl: px(config.radius * 2.),
                ..Default::default()
            },
            // Sizes and line heights are gpui-base's scale on purpose.
            typography: TypographyTokens {
                sans: config.font_family.clone().into(),
                mono: config.mono_font_family.clone().into(),
                ..Default::default()
            },
            ..Default::default()
        };

        Ok(Self {
            accent,
            background,
            border,
            danger,
            danger_active: danger,
            danger_foreground,
            font_family: config.font_family.into(),
            foreground,
            highlight_theme: if mode.is_dark() {
                HighlightTheme::default_dark()
            } else {
                HighlightTheme::default_light()
            },
            info: color("info.background")?,
            info_foreground: color("info.foreground")?,
            input,
            link: primary,
            list_active: color("list.active.background")?,
            list_hover: color("list.hover.background")?,
            mode,
            mono_font_family: config.mono_font_family.into(),
            muted,
            muted_foreground,
            popover,
            primary,
            primary_foreground,
            ring,
            scrollbar: color("scrollbar.background")?,
            scrollbar_thumb: color("scrollbar.thumb.background")?,
            scrollbar_thumb_hover: color("scrollbar.thumb.hover.background")?,
            secondary,
            secondary_active: secondary,
            selection: primary.alpha(0.3),
            sidebar: color("sidebar.background")?,
            sidebar_accent: color("sidebar.accent.background")?,
            sidebar_foreground: color("sidebar.foreground")?,
            success: color("success.background")?,
            success_foreground: color("success.foreground")?,
            tab_active: background,
            theme_name: config.name.clone().into(),
            tokens,
            warning: color("warning.background")?,
            warning_foreground: color("warning.foreground")?,
        })
    }
}

/// Initialize gpui-base behavior and tcode's application-owned theme.
pub fn init(cx: &mut App) {
    init_with_json(TCODE_THEME, cx);
}

/// Initialize from a caller-supplied copy of the embedded theme.
///
/// The desktop app uses this to flatten its translucent canvas colors when
/// the platform window is opaque.
pub fn init_with_json(theme_json: &str, cx: &mut App) {
    gpui_base::init(cx);
    crate::overlay::init(cx);
    crate::widgets::menu::init(cx);
    let themes = parse_theme_file(theme_json).expect("embedded themes/tcode.json must be valid");

    cx.set_global(themes.clone());
    change_mode(ThemeMode::Light, None, cx);
}

pub fn change_mode(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    let theme = match mode {
        ThemeMode::Light => cx.global::<Themes>().light.clone(),
        ThemeMode::Dark => cx.global::<Themes>().dark.clone(),
    };
    let radius = theme.tokens.radius;
    let base_theme = gpui_base::Theme {
        appearance: match mode {
            ThemeMode::Light => ThemeAppearance::Light,
            ThemeMode::Dark => ThemeAppearance::Dark,
        },
        tokens: theme.tokens.clone(),
        scrollbar: ScrollbarTheme::new()
            .with_mode(if cx.should_auto_hide_scrollbars() {
                ScrollbarMode::Scrolling
            } else {
                ScrollbarMode::Hover
            })
            .with_styles(
                ScrollbarStyles::default()
                    .track(|style| style.bg(theme.scrollbar))
                    .track_hover(|style| style.bg(theme.scrollbar))
                    .track_active(|style| style.bg(theme.scrollbar).border_color(theme.border))
                    .thumb(|style| style.bg(theme.scrollbar_thumb).radius(radius.full))
                    .thumb_hover(|style| style.bg(theme.scrollbar_thumb_hover).radius(radius.full))
                    .thumb_active(|style| {
                        style.bg(theme.scrollbar_thumb_hover).radius(radius.full)
                    }),
            ),
        resizable: ResizableTheme::default(),
        plot: PlotTheme::default(),
    };
    cx.set_global(base_theme);
    cx.set_global(theme);
    #[cfg(target_os = "macos")]
    crate::macos_backdrop::sync_appearance(mode, cx);
    if let Some(window) = window {
        window.refresh();
    }
}

pub fn sync_system_appearance(window: Option<&mut Window>, cx: &mut App) {
    let appearance = window
        .as_ref()
        .map(|window| window.appearance())
        .unwrap_or_else(|| cx.window_appearance());
    change_mode(appearance.into(), window, cx);
}
