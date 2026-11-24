use std::{
    collections::BTreeMap,
    collections::HashSet,
    ops::Range,
    panic::{self, AssertUnwindSafe},
    path::Path,
    sync::{Arc, LazyLock, Mutex, PoisonError},
};

use gpui::{FontStyle, FontWeight, HighlightStyle, Hsla, Rgba};
use syntect::{
    easy::ScopeRangeIterator,
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};

/// Named syntax styles resolved from the bundled light or dark palette.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HighlightTheme {
    styles: BTreeMap<&'static str, ThemeStyle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ThemeStyle {
    color: Option<Hsla>,
    font_style: Option<FontStyle>,
    font_weight: Option<FontWeight>,
}

impl ThemeStyle {
    fn color(value: &str) -> Self {
        Self {
            color: Some(
                Rgba::try_from(value)
                    .expect("built-in highlight color should parse")
                    .into(),
            ),
            font_style: None,
            font_weight: None,
        }
    }

    const fn italic() -> Self {
        Self {
            color: None,
            font_style: Some(FontStyle::Italic),
            font_weight: None,
        }
    }

    const fn bold(weight: FontWeight) -> Self {
        Self {
            color: None,
            font_style: None,
            font_weight: Some(weight),
        }
    }
}

impl From<ThemeStyle> for HighlightStyle {
    fn from(style: ThemeStyle) -> Self {
        Self {
            color: style.color,
            font_style: style.font_style,
            font_weight: style.font_weight,
            ..Default::default()
        }
    }
}

impl HighlightTheme {
    pub fn default_light() -> Arc<Self> {
        static LIGHT: LazyLock<Arc<HighlightTheme>> =
            LazyLock::new(|| Arc::new(HighlightTheme::from_styles(LIGHT_STYLES)));
        LIGHT.clone()
    }

    pub fn default_dark() -> Arc<Self> {
        static DARK: LazyLock<Arc<HighlightTheme>> =
            LazyLock::new(|| Arc::new(HighlightTheme::from_styles(DARK_STYLES)));
        DARK.clone()
    }

    pub fn style(&self, name: &str) -> Option<HighlightStyle> {
        self.styles
            .get(name)
            .or_else(|| {
                name.split_once('.')
                    .and_then(|(prefix, _)| self.styles.get(prefix))
            })
            .copied()
            .map(Into::into)
    }

    fn from_styles(styles: &[(&'static str, StyleValue)]) -> Self {
        Self {
            styles: styles
                .iter()
                .map(|&(name, value)| {
                    let style = match value {
                        StyleValue::Color(color) => ThemeStyle::color(color),
                        StyleValue::Italic => ThemeStyle::italic(),
                        StyleValue::Weight(weight) => ThemeStyle::bold(weight),
                        StyleValue::ColorWithStyle(color, font_style) => ThemeStyle {
                            color: ThemeStyle::color(color).color,
                            font_style: Some(font_style),
                            font_weight: None,
                        },
                        StyleValue::ColorWithWeight(color, font_weight) => ThemeStyle {
                            color: ThemeStyle::color(color).color,
                            font_style: None,
                            font_weight: Some(font_weight),
                        },
                    };
                    (name, style)
                })
                .collect(),
        }
    }
}

impl gpui_base::input::HighlightStyleResolver for HighlightTheme {
    fn style(&self, name: &str) -> Option<HighlightStyle> {
        self.style(name)
    }
}

#[derive(Clone, Copy)]
enum StyleValue {
    Color(&'static str),
    Italic,
    Weight(FontWeight),
    ColorWithStyle(&'static str, FontStyle),
    ColorWithWeight(&'static str, FontWeight),
}

use StyleValue::{Color, ColorWithStyle, ColorWithWeight, Italic, Weight};

// Values are the upstream styled layer's Default Light/Dark syntax palettes.
const LIGHT_STYLES: &[(&str, StyleValue)] = &[
    ("attribute", Color("#957931")),
    ("boolean", Color("#C5060B")),
    ("comment", Color("#007fff")),
    ("comment.doc", Color("#007fff")),
    ("constant", Color("#C5060B")),
    ("constructor", Color("#0433ff")),
    ("embedded", Color("#333333")),
    ("emphasis", Italic),
    ("emphasis.strong", Weight(FontWeight::BOLD)),
    ("function", Color("#0000A2")),
    ("keyword", Color("#0433ff")),
    ("link_text", ColorWithStyle("#0000A2", FontStyle::Normal)),
    ("link_uri", ColorWithStyle("#6A7293", FontStyle::Italic)),
    ("number", Color("#0433ff")),
    ("property", Color("#333333")),
    ("string", Color("#036A07")),
    ("string.escape", Color("#036A07")),
    ("string.regex", Color("#036A07")),
    ("string.special", Color("#d21f07")),
    ("string.special.symbol", Color("#d21f07")),
    ("tag", Color("#0433ff")),
    ("text.code.span", Color("#6F42C1")),
    ("text.literal", Color("#6F42C1")),
    ("title", Color("#0433FF")),
    ("type", Color("#6f42c1")),
    ("variable", Color("#333333")),
    ("variable.special", Color("#C5060B")),
];

const DARK_STYLES: &[(&str, StyleValue)] = &[
    ("attribute", Color("#e7cb8f")),
    ("boolean", Color("#E1D797")),
    ("comment", Color("#9E9E9E")),
    ("comment.doc", Color("#9E9E9E")),
    ("constant", Color("#E1D797")),
    ("constructor", Color("#b5af9a")),
    ("embedded", Color("#CACCCA")),
    ("emphasis", Italic),
    ("emphasis.strong", Weight(FontWeight::BOLD)),
    ("function", Color("#fdd888")),
    ("keyword", Color("#c28b12")),
    ("link_text", ColorWithStyle("#307BF6", FontStyle::Normal)),
    ("link_uri", ColorWithStyle("#7faef9", FontStyle::Italic)),
    ("number", Color("#E1D797")),
    ("property", Color("#CACCCA")),
    ("string", Color("#62BA46")),
    ("string.escape", Color("#62BA46")),
    ("string.regex", Color("#62BA46")),
    ("string.special", Color("#E1D797")),
    ("string.special.symbol", Color("#E1D797")),
    ("tag", Color("#b5af9a")),
    ("text.code.span", Color("#E1D797")),
    ("text.literal", Color("#E1D797")),
    ("title", ColorWithWeight("#fdd888", FontWeight::SEMIBOLD)),
    ("type", Color("#c75828")),
    ("variable.special", Color("#E19773")),
];

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(|| {
    syntect::dumps::from_uncompressed_data(include_bytes!("../assets/syntaxes.bin"))
        .expect("failed to load syntect syntax set from crates/ui/assets/syntaxes.bin")
});

/// Return the syntax name associated with `path`, or `"text"` when its
/// extension is unknown.
pub(crate) fn language_name_for_path(path: &str) -> &'static str {
    let syntax = SYNTAX_SET
        .find_syntax_for_file(Path::new(path))
        .ok()
        .flatten();

    match syntax.map(|syntax| syntax.name.as_str()) {
        Some("Rust") => "rust",
        Some("Python") => "python",
        Some("TypeScript") => "typescript",
        Some("Plain Text") | None => "text",
        // Syntax names are owned by the process-wide syntax set, so this
        // reference has the same lifetime as the static set.
        Some(name) => name,
    }
}

/// Resolve a Markdown fence token, language name, or file extension.
pub(crate) fn syntax_for_name_or_extension(name: &str) -> Option<&'static SyntaxReference> {
    let token = name.trim();
    if token.is_empty() {
        return None;
    }
    SYNTAX_SET
        .find_syntax_by_token(token)
        .or_else(|| SYNTAX_SET.find_syntax_by_extension(token))
}

/// TextMate scope prefixes ordered from most specific to least specific.
///
/// Each scope in a token's stack is examined from innermost to outermost. If
/// the theme has no style for a matching key, matching continues so a broader
/// scope can supply a style.
const SCOPE_TO_THEME_KEY: &[(&str, &str)] = &[
    ("comment.documentation", "comment.doc"),
    ("constant.character.escape", "string.escape"),
    ("constant.character", "string"),
    ("string.regexp", "string.regex"),
    ("string.quoted", "string"),
    ("string.unquoted", "string"),
    ("punctuation.section.brackets", "punctuation.bracket"),
    ("punctuation.section.braces", "punctuation.bracket"),
    ("punctuation.section.parens", "punctuation.bracket"),
    ("punctuation.separator", "punctuation.delimiter"),
    ("punctuation.terminator", "punctuation.delimiter"),
    ("variable.other.member", "property"),
    ("variable.other.property", "property"),
    ("entity.other.attribute-name", "attribute"),
    ("keyword.control.preprocessor", "preproc"),
    ("keyword.control.import", "keyword"),
    ("keyword.control", "keyword"),
    ("keyword.operator", "operator"),
    ("storage.modifier", "keyword"),
    ("storage.type.function", "keyword"),
    ("storage.type", "type"),
    ("entity.name.function", "function"),
    ("support.function", "function"),
    ("entity.name.type", "type"),
    ("entity.name.class", "type"),
    ("entity.name.struct", "type"),
    ("entity.name.enum", "type"),
    ("support.type", "type"),
    ("entity.name.tag", "tag"),
    ("entity.name.section", "title"),
    ("markup.heading", "title"),
    ("markup.bold", "emphasis.strong"),
    ("markup.italic", "emphasis"),
    ("markup.raw", "text.literal"),
    ("entity.name.label", "label"),
    ("support.class", "constructor"),
    ("support.constant", "constructor"),
    ("meta.preprocessor", "preproc"),
    ("constant.numeric", "number"),
    ("constant.language", "boolean"),
    ("constant", "constant"),
    ("variable", "variable"),
    ("keyword", "keyword"),
    ("comment", "comment"),
    ("string", "string"),
    ("punctuation", "punctuation"),
];

fn scope_matches(scope: &str, selector: &str) -> bool {
    scope == selector
        || scope
            .strip_prefix(selector)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

fn style_for_stack(stack: &ScopeStack, theme: &HighlightTheme) -> HighlightStyle {
    for scope in stack.scopes.iter().rev() {
        let scope = scope.to_string();
        for &(selector, key) in SCOPE_TO_THEME_KEY {
            if scope_matches(&scope, selector)
                && let Some(style) = theme.style(key)
            {
                return style;
            }
        }
    }
    HighlightStyle::default()
}

fn push_merged(
    runs: &mut Vec<(Range<usize>, HighlightStyle)>,
    range: Range<usize>,
    style: HighlightStyle,
) {
    if range.is_empty() {
        return;
    }
    if let Some((previous_range, previous_style)) = runs.last_mut()
        && previous_range.end == range.start
        && *previous_style == style
    {
        previous_range.end = range.end;
        return;
    }
    runs.push((range, style));
}

/// Syntaxes whose parsing has panicked; they render unhighlighted afterwards.
///
/// syntect compiles each grammar regex lazily on first use and panics when one
/// fails to compile, and its parser has further internal panic paths. A panic
/// mid-render unwinds through GPUI's element tree and has crashed the app with
/// a double panic (see `examples/sanitize_syntaxes.rs`), so a syntax that
/// panicked once is never handed to syntect again in this process.
static POISONED_SYNTAXES: LazyLock<Mutex<HashSet<&'static str>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug)]
pub(crate) struct HighlightedSource {
    pub(crate) runs: Vec<(Range<usize>, HighlightStyle)>,
    source: gpui::SharedString,
    language: gpui::SharedString,
    theme: HighlightTheme,
    checkpoint: Option<HighlightCheckpoint>,
}

#[derive(Debug, Clone)]
struct HighlightCheckpoint {
    offset: usize,
    parse: ParseState,
    scopes: ScopeStack,
    runs_len: usize,
    last_run_end: usize,
}

impl HighlightedSource {
    pub(crate) fn new(
        source: gpui::SharedString,
        language: gpui::SharedString,
        theme: &HighlightTheme,
        previous: Option<&Self>,
    ) -> Self {
        let previous = previous.filter(|old| {
            old.language == language
                && old.theme == *theme
                && old
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| source.starts_with(&old.source[..checkpoint.offset]))
        });
        let parsed = highlight_safely(&language, |syntax| {
            highlight_from(&source, syntax, theme, previous)
        });
        let (runs, checkpoint) = parsed.unwrap_or_else(|| (plain_runs(&source), None));
        Self {
            runs,
            source,
            language,
            theme: theme.clone(),
            checkpoint,
        }
    }
}

fn plain_runs(src: &str) -> Vec<(Range<usize>, HighlightStyle)> {
    if src.is_empty() {
        Vec::new()
    } else {
        vec![(0..src.len(), HighlightStyle::default())]
    }
}

/// Highlight a complete source string, returning original UTF-8 byte ranges.
pub(crate) fn highlight_source(
    src: &str,
    lang: &str,
    theme: &HighlightTheme,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if src.is_empty() {
        return Vec::new();
    }
    highlight_safely(lang, |syntax| highlight_with_syntect(src, syntax, theme))
        .unwrap_or_else(|| plain_runs(src))
}

fn highlight_safely<T>(lang: &str, compute: impl FnOnce(&SyntaxReference) -> T) -> Option<T> {
    let syntax = syntax_for_name_or_extension(lang)?;
    let name = syntax.name.as_str();
    // A poisoned set must never re-run a syntax whose parser already panicked.
    if POISONED_SYNTAXES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(name)
    {
        return None;
    }
    match panic::catch_unwind(AssertUnwindSafe(|| compute(syntax))) {
        Ok(value) => Some(value),
        Err(_) => {
            log::error!("syntect panicked highlighting {name:?}; disabling it for this session");
            POISONED_SYNTAXES
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(name);
            None
        }
    }
}

fn highlight_with_syntect(
    src: &str,
    syntax: &SyntaxReference,
    theme: &HighlightTheme,
) -> Vec<(Range<usize>, HighlightStyle)> {
    highlight_from(src, syntax, theme, None).0
}

fn highlight_from(
    src: &str,
    syntax: &SyntaxReference,
    theme: &HighlightTheme,
    previous: Option<&HighlightedSource>,
) -> (
    Vec<(Range<usize>, HighlightStyle)>,
    Option<HighlightCheckpoint>,
) {
    let saved = previous.and_then(|old| old.checkpoint.as_ref());
    let mut parse_state =
        saved.map_or_else(|| ParseState::new(syntax), |saved| saved.parse.clone());
    let mut scope_stack = saved.map_or_else(ScopeStack::new, |saved| saved.scopes.clone());
    let mut runs = match (previous, saved) {
        (Some(old), Some(saved)) => {
            let mut runs = old.runs[..saved.runs_len].to_vec();
            if let Some((range, _)) = runs.last_mut() {
                range.end = saved.last_run_end;
            }
            runs
        }
        _ => Vec::new(),
    };
    let mut line_start = saved.map_or(0, |saved| saved.offset);
    let mut checkpoint = saved.cloned();

    for line in LinesWithEndings::from(&src[line_start..]) {
        let parse_checkpoint = parse_state.clone();
        let stack_checkpoint = scope_stack.clone();
        // Reparse the final line on growth: its tokens and newline-dependent
        // grammar transitions may change when the next chunk arrives.
        if line_start + line.len() == src.len() {
            checkpoint = Some(HighlightCheckpoint {
                offset: line_start,
                parse: parse_checkpoint.clone(),
                scopes: stack_checkpoint.clone(),
                runs_len: runs.len(),
                last_run_end: runs.last().map_or(0, |(range, _)| range.end),
            });
        }
        let Ok(ops) = parse_state.parse_line(line, &SYNTAX_SET) else {
            parse_state = parse_checkpoint;
            push_merged(
                &mut runs,
                line_start..line_start + line.len(),
                HighlightStyle::default(),
            );
            line_start += line.len();
            continue;
        };

        let mut line_runs = Vec::new();
        let mut offset = 0;
        let mut valid = true;
        for (range, op) in ScopeRangeIterator::new(&ops, line) {
            if scope_stack.apply(op).is_err() {
                valid = false;
                break;
            }
            if !range.is_empty() {
                let start = line_start + offset;
                let end = start + range.len();
                push_merged(
                    &mut line_runs,
                    start..end,
                    style_for_stack(&scope_stack, theme),
                );
                offset += range.len();
            }
        }

        if valid && offset == line.len() {
            for (range, style) in line_runs {
                push_merged(&mut runs, range, style);
            }
        } else {
            scope_stack = stack_checkpoint;
            parse_state = parse_checkpoint;
            push_merged(
                &mut runs,
                line_start..line_start + line.len(),
                HighlightStyle::default(),
            );
        }
        line_start += line.len();
    }

    (runs, checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_language_from_path() {
        assert_eq!(language_name_for_path("x.rs"), "rust");
        assert_eq!(language_name_for_path("x.py"), "python");
        assert_eq!(language_name_for_path("x.ts"), "typescript");
        assert_eq!(language_name_for_path("x.tsx"), "TypeScriptReact");
        assert_eq!(language_name_for_path("x.toml"), "TOML");
        assert_eq!(language_name_for_path("x.kt"), "Kotlin");
        assert_eq!(language_name_for_path("x.swift"), "Swift");
        assert_eq!(language_name_for_path("x.ex"), "Elixir");
        assert_eq!(language_name_for_path("x.zig"), "Zig");
        assert_eq!(language_name_for_path("noext"), "text");
        assert!(SYNTAX_SET.find_syntax_by_name("Plain Text").is_some());
    }

    /// syntect keeps `substitute_backrefs_in_regex` private, so the probe
    /// substitutes back-references with the placeholder its YAML loader
    /// validates patterns with, as `examples/sanitize_syntaxes.rs` does when it
    /// regenerates the dump.
    fn substitute_backrefs(regex_str: &str) -> String {
        let mut result = String::with_capacity(regex_str.len());
        let mut last_was_escape = false;
        for c in regex_str.chars() {
            if last_was_escape && c.is_ascii_digit() {
                result.push_str("<placeholder>");
            } else if last_was_escape {
                result.push('\\');
                result.push(c);
            } else if c != '\\' {
                result.push(c);
            }
            last_was_escape = c == '\\' && !last_was_escape;
        }
        if last_was_escape {
            result.push('\\');
        }
        result
    }

    /// Every pattern syntect will compile must compile under the backend this
    /// crate ships with — raw for ordinary patterns, after back-reference
    /// placeholder substitution for capture patterns (matching syntect's own
    /// YAML-loader validation). Guards `assets/syntaxes.bin` regressions: a
    /// pattern failing here is a deferred runtime panic (syntect `regex.rs`
    /// "regex string should be pre-tested"). Regenerate the dump with
    /// `examples/sanitize_syntaxes.rs` when this fails.
    #[test]
    fn shipped_syntax_set_compiles_under_active_regex_backend() {
        use syntect::parsing::{Regex, SyntaxSet, syntax_definition::Pattern};

        let set: SyntaxSet =
            syntect::dumps::from_uncompressed_data(include_bytes!("../assets/syntaxes.bin"))
                .expect("load shipped dump");
        let mut failures = Vec::new();
        // Compilation depends only on the pattern, which many syntaxes share.
        let mut checked = HashSet::new();
        for syntax in set.into_builder().syntaxes() {
            if let Some(first_line) = &syntax.first_line_match
                && checked.insert(first_line.clone())
                && let Some(err) = Regex::try_compile(first_line)
            {
                failures.push(format!("{}: first_line_match: {err}", syntax.name));
            }
            for context in syntax.contexts.values() {
                for pattern in &context.patterns {
                    let Pattern::Match(pattern) = pattern else {
                        continue;
                    };
                    let probe = if pattern.has_captures {
                        substitute_backrefs(pattern.regex.regex_str())
                    } else {
                        pattern.regex.regex_str().to_string()
                    };
                    if checked.insert(probe.clone())
                        && let Some(err) = Regex::try_compile(&probe)
                    {
                        failures.push(format!("{}: {err}", syntax.name));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "uncompilable patterns:\n{}",
            failures.join("\n")
        );

        // Exercise the known lazy JavaScript (Babel) parser panic without the
        // production catch_unwind fallback masking an incompatible dump.
        let src = "const f = async (a, b) => a + b;\n";
        let syntax = syntax_for_name_or_extension("jsx").expect("jsx syntax");
        assert_eq!(syntax.name, "JavaScript (Babel)");
        let runs = highlight_with_syntect(src, syntax, &HighlightTheme::default_dark());
        assert!(!runs.is_empty());
        assert!(runs.iter().all(|(range, _)| range.end <= src.len()));
    }

    #[test]
    fn source_highlights_cover_utf8_ranges_and_preserve_unknown_language_text() {
        {
            let src = "fn ordinary() { let 名称 = \"中\"; }\n";
            let theme = HighlightTheme::default_dark();
            let runs = highlight_source(src, "rust", &theme);
            let keyword_style = theme.style("keyword").expect("default theme has keywords");

            let fn_style = runs
                .iter()
                .find(|(range, _)| range.start == 0 && range.end >= 2)
                .map(|(_, style)| *style)
                .expect("fn is covered");
            let identifier_start = src.find("ordinary").unwrap();
            let identifier_style = runs
                .iter()
                .find(|(range, _)| range.start <= identifier_start && range.end > identifier_start)
                .map(|(_, style)| *style)
                .expect("identifier is covered");

            assert_eq!(fn_style.color, keyword_style.color);
            assert_ne!(identifier_style, keyword_style);
            assert!(runs.iter().all(|(range, _)| range.start < range.end
                && range.end <= src.len()
                && src.is_char_boundary(range.start)
                && src.is_char_boundary(range.end)));
            assert!(runs.windows(2).all(|pair| pair[0].0.end <= pair[1].0.start));
        }
        {
            let src = "some source";
            assert_eq!(
                highlight_source(src, "unknown-language", &HighlightTheme::default_dark()),
                vec![(0..src.len(), HighlightStyle::default())]
            );
        }
    }
}
