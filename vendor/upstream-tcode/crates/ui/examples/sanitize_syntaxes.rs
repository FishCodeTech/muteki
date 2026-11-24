//! Sanitize a syntect syntax-set dump for the fancy-regex backend.
//!
//! `assets/syntaxes.bin` originates from bat, whose syntax set is pre-tested
//! against the Oniguruma backend. tcode compiles syntect with `regex-fancy`,
//! and fancy-regex rejects some Oniguruma-only constructs (subroutine calls,
//! variable-length look-behind, `\o{}` escapes, ...). syntect compiles each
//! pattern lazily on first use and panics on failure (`regex.rs:70`,
//! "regex string should be pre-tested"), which crashed the app in the field.
//!
//! This tool replaces every pattern that fails to compile under the active
//! backend with `(?!)` — valid, and it never matches — so the affected rule is
//! disabled instead of being a runtime landmine. Patterns with `\N` capture
//! back-references are validated the way syntect's own YAML loader does
//! (`try_compile_regex`): back-references are swapped for a literal
//! placeholder first, because syntect substitutes the captured text before
//! compiling those patterns at runtime.
//!
//! The rebuilt set drops syntect's private path→syntax mapping (only used by
//! `find_syntax_by_path`, which tcode never calls) and carries no metadata
//! (the `metadata` feature is off).
//!
//! Usage: cargo run -p tcode-ui --example sanitize_syntaxes -- <in.bin> <out.bin>

use syntect::parsing::syntax_definition::Pattern;
use syntect::parsing::{Regex, SyntaxSet, SyntaxSetBuilder};

const NEVER_MATCHES: &str = "(?!)";

/// Mirror of syntect's private `substitute_backrefs_in_regex`
/// (`syntax_definition.rs`), with the placeholder substituter its YAML loader
/// uses for compile validation. Escaped literal text substituted at runtime
/// cannot introduce regex operators, so a placeholder is the right probe.
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

/// The compile error syntect would eventually hit for this pattern, if any.
fn compile_error(
    regex_str: &str,
    has_captures: bool,
) -> Option<Box<dyn std::error::Error + Send + Sync>> {
    if has_captures {
        Regex::try_compile(&substitute_backrefs(regex_str))
    } else {
        Regex::try_compile(regex_str)
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .expect("usage: sanitize_syntaxes <in.bin> <out.bin>");
    let output = args
        .next()
        .expect("usage: sanitize_syntaxes <in.bin> <out.bin>");

    assert!(
        Regex::try_compile(NEVER_MATCHES).is_none(),
        "replacement pattern must compile under the active regex backend"
    );
    assert!(!Regex::new(NEVER_MATCHES.into()).is_match("probe"));

    let data = std::fs::read(&input).expect("read input dump");
    let set: SyntaxSet =
        syntect::dumps::from_uncompressed_data(&data).expect("deserialize input dump");
    let syntax_count = set.syntaxes().len();

    let mut replaced = 0usize;
    let mut builder = SyntaxSetBuilder::new();
    for syntax in set.into_builder().syntaxes() {
        let mut syntax = syntax.clone();
        if let Some(first_line) = &syntax.first_line_match
            && let Some(err) = Regex::try_compile(first_line)
        {
            println!("{}: first_line_match: {err}", syntax.name);
            syntax.first_line_match = None;
            replaced += 1;
        }
        for context in syntax.contexts.values_mut() {
            for pattern in &mut context.patterns {
                let Pattern::Match(pattern) = pattern else {
                    continue;
                };
                if let Some(err) = compile_error(pattern.regex.regex_str(), pattern.has_captures) {
                    println!(
                        "{}{}: {err}: {}",
                        syntax.name,
                        if pattern.has_captures {
                            " (backref-substituted)"
                        } else {
                            ""
                        },
                        pattern.regex.regex_str().escape_debug()
                    );
                    pattern.regex = Regex::new(NEVER_MATCHES.into());
                    replaced += 1;
                }
            }
        }
        builder.add(syntax);
    }

    let sanitized = builder.build();
    assert_eq!(
        sanitized.syntaxes().len(),
        syntax_count,
        "syntaxes lost in rebuild"
    );
    // Uncompressed on purpose: highlight.rs loads this via
    // `from_uncompressed_data` (the lazy per-syntax context blobs inside are
    // already compressed, so an outer zlib layer only costs startup time).
    syntect::dumps::dump_to_uncompressed_file(&sanitized, &output).expect("write output dump");

    // Prove the shipped bytes are clean: reload the file we just wrote and
    // re-run the compile check over every pattern.
    let bytes = std::fs::read(&output).expect("re-read output dump");
    let reloaded: SyntaxSet =
        syntect::dumps::from_uncompressed_data(&bytes).expect("reload sanitized dump");
    let mut remaining = 0usize;
    for syntax in reloaded.into_builder().syntaxes() {
        if let Some(first_line) = &syntax.first_line_match
            && Regex::try_compile(first_line).is_some()
        {
            remaining += 1;
        }
        for context in syntax.contexts.values() {
            for pattern in &context.patterns {
                if let Pattern::Match(pattern) = pattern
                    && compile_error(pattern.regex.regex_str(), pattern.has_captures).is_some()
                {
                    remaining += 1;
                }
            }
        }
    }
    assert_eq!(
        remaining, 0,
        "sanitized set still has uncompilable patterns"
    );

    println!("replaced {replaced} patterns across {syntax_count} syntaxes -> {output}");
}
