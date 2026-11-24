//! Claude Code's model-id grammar for the context window, shared by process
//! and type-only builds.

/// Claude Code's context-window model suffixes (`claude-opus-5[1m]`). The
/// CLI compares model ids with `/\[1m\]$/i` and its canonical-name
/// normalizer already accepts `[2m]`; this list is deliberately explicit
/// rather than matching any `[...]`.
pub const CONTEXT_WINDOW_SUFFIXES: &[&str] = &["[1m]", "[2m]"];

/// Drop a listed Claude Code context suffix from a model id, case-
/// insensitively as the CLI does. The CLI echoes the launch id
/// (`claude-opus-5[1m]`) in `init` while the API reports the bare id, and
/// the two name one model.
pub fn strip_context_window_suffix(model: &str) -> &str {
    CONTEXT_WINDOW_SUFFIXES
        .iter()
        .find_map(|suffix| {
            let base = model.len().checked_sub(suffix.len())?;
            model
                .is_char_boundary(base)
                .then(|| model.split_at(base))
                .filter(|(_, tail)| tail.eq_ignore_ascii_case(suffix))
                .map(|(head, _)| head)
        })
        .unwrap_or(model)
}
