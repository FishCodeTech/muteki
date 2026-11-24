use rushdown::{
    as_type_data_mut,
    ast::{Arena, KindData, NodeKind, NodeRef, NodeType, PrettyPrint},
    parser::{BlockParser, Context, InlineParser, NoParserOptions, Parser, ParserExtension, State},
    text::{BasicReader, BlockReader, Reader},
};

#[derive(Debug, Clone)]
pub(super) struct MathSpan {
    pub source: String,
    pub display: bool,
    pub selection: super::math_layout::FormulaSelection,
}

impl PartialEq for MathSpan {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.display == other.display
    }
}

impl NodeKind for MathSpan {
    fn typ(&self) -> NodeType {
        NodeType::Inline
    }
    fn kind_name(&self) -> &'static str {
        "Math"
    }
}

impl PrettyPrint for MathSpan {
    fn pretty_print(&self, w: &mut dyn std::fmt::Write, _: &str, _: usize) -> std::fmt::Result {
        w.write_str(&self.source)
    }
}

#[derive(Debug)]
pub(super) struct MathParser;

impl ParserExtension for MathParser {
    fn apply(self, parser: &mut Parser) {
        parser.add_inline_parser(
            || Box::new(MathParser) as Box<dyn InlineParser>,
            NoParserOptions,
            50,
        );
    }
}

impl InlineParser for MathParser {
    fn trigger(&self) -> &[u8] {
        b"$\\"
    }

    fn parse(
        &self,
        arena: &mut Arena,
        _: NodeRef,
        reader: &mut BlockReader,
        _: &mut Context,
    ) -> Option<NodeRef> {
        let (line, _) = reader.peek_line()?;
        let (open, close, display) = if line.starts_with("$$") {
            ("$$", "$$", true)
        } else if line.starts_with('$') && !line[1..].starts_with(char::is_whitespace) {
            ("$", "$", false)
        } else if line.starts_with(r"\[") {
            (r"\[", r"\]", true)
        } else if line.starts_with(r"\(") {
            (r"\(", r"\)", false)
        } else {
            return None;
        };
        let saved = reader.position();
        reader.advance(open.len());
        let mut source = String::new();
        while let Some((line, _)) = reader.peek_line() {
            let mut escaped = false;
            for (ix, ch) in line.char_indices() {
                if !escaped
                    && line[ix..].starts_with(close)
                    && (close != "$"
                        || (ix > 0
                            && !line[..ix].ends_with(char::is_whitespace)
                            && !line[ix + 1..].starts_with(|c: char| c.is_ascii_digit())))
                {
                    source.push_str(&line[..ix]);
                    reader.advance(ix + close.len());
                    return Some(arena.new_node(KindData::Extension(Box::new(MathSpan {
                        source,
                        display,
                        selection: Default::default(),
                    }))));
                }
                escaped = ch == '\\' && !escaped;
            }
            if !display {
                break;
            }
            source.push_str(&line);
            reader.advance_line();
        }
        reader.set_position(saved.0, saved.1);
        None
    }
}

/// A display formula that starts a line (`\[` or `$$`) is a block of its own,
/// so the lines up to its closing delimiter are never read as paragraph
/// structure: a lone `=` or `---` inside the formula would otherwise turn the
/// paragraph into a setext heading or a thematic break and split the formula.
/// The block keeps its delimiters and is parsed inline like a paragraph, so an
/// unclosed formula stays visible as text while the message streams in.
#[derive(Debug)]
pub(super) struct MathBlock;

impl NodeKind for MathBlock {
    fn typ(&self) -> NodeType {
        NodeType::LeafBlock
    }
    fn kind_name(&self) -> &'static str {
        "MathBlock"
    }
}

impl PrettyPrint for MathBlock {
    fn pretty_print(&self, _: &mut dyn std::fmt::Write, _: &str, _: usize) -> std::fmt::Result {
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct MathBlockParser;

impl ParserExtension for MathBlockParser {
    fn apply(self, parser: &mut Parser) {
        parser.add_block_parser(
            || Box::new(MathBlockParser) as Box<dyn BlockParser>,
            NoParserOptions,
            50,
        );
    }
}

fn block_delimiters(line: &str) -> Option<(&'static str, &'static str)> {
    if line.starts_with("$$") {
        Some(("$$", "$$"))
    } else if line.starts_with(r"\[") {
        Some((r"\[", r"\]"))
    } else {
        None
    }
}

/// The byte offset just past an unescaped `close` in `line`, if any.
fn find_close(line: &str, close: &str) -> Option<usize> {
    let mut escaped = false;
    for (ix, ch) in line.char_indices() {
        if !escaped && line[ix..].starts_with(close) {
            return Some(ix + close.len());
        }
        escaped = ch == '\\' && !escaped;
    }
    None
}

impl BlockParser for MathBlockParser {
    fn trigger(&self) -> &[u8] {
        b"$\\"
    }

    fn open(
        &self,
        arena: &mut Arena,
        _: NodeRef,
        reader: &mut BasicReader,
        ctx: &mut Context,
    ) -> Option<(NodeRef, State)> {
        let (line, segment) = reader.peek_line()?;
        let rest = &line[ctx.block_offset()?..];
        let (open, close) = block_delimiters(rest)?;
        let body = &rest[open.len()..];
        if let Some(end) = find_close(body, close)
            && !body[end..].trim().is_empty()
        {
            return None;
        }
        let node_ref = arena.new_node(KindData::Extension(Box::new(MathBlock)));
        as_type_data_mut!(arena, node_ref, Block).append_source_line(segment);
        reader.advance_to_eol();
        Some((node_ref, State::NO_CHILDREN))
    }

    fn cont(
        &self,
        arena: &mut Arena,
        node_ref: NodeRef,
        reader: &mut BasicReader,
        _: &mut Context,
    ) -> Option<State> {
        let (_, segment) = reader.peek_line()?;
        let block = as_type_data_mut!(arena, node_ref, Block);
        let lines = block.source();
        let first = lines.first()?.str(reader.source());
        let (open, close) = block_delimiters(first.trim_start())?;
        let last = lines.last()?.str(reader.source());
        let body = if lines.len() == 1 {
            &first.trim_start()[open.len()..]
        } else {
            &last
        };
        if find_close(body, close).is_some() {
            return None;
        }
        block.append_source_line(segment);
        reader.advance_to_eol();
        Some(State::NO_CHILDREN)
    }

    fn can_interrupt_paragraph(&self) -> bool {
        true
    }
}
