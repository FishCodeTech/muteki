use rushdown::parser::ParserExtension as _;
use std::borrow::Cow;

use rushdown::ast::{
    Arena, CodeBlockKind, KindData, NodeRef, TableCellAlignment, Task, TextQualifier,
};

use super::nodes::{
    BlockNode, CodeBlock, ColumnumnAlign, ImageNode, InlineNode, LinkMark, Paragraph, Table,
    TableCell, TableRow, TextMark,
};

pub(crate) struct ParsedDocument {
    pub(crate) root: BlockNode,
    pub(crate) root_starts: Option<Vec<usize>>,
}

#[cfg(test)]
pub(crate) fn parse(source: &str) -> BlockNode {
    parse_document(source).root
}

pub(crate) fn parse_document(source: &str) -> ParsedDocument {
    let parser = rushdown::parser::Parser::with_extensions(
        rushdown::parser::Options::default(),
        rushdown::parser::gfm(rushdown::parser::GfmOptions::default())
            .and(super::math_parse::MathParser)
            .and(super::math_parse::MathBlockParser),
    );
    let mut reader = rushdown::text::BasicReader::new(source);
    let (arena, doc_ref) = parser.parse(&mut reader);
    let mut root_starts = Vec::new();
    let mut positions_available = true;
    let children = arena[doc_ref]
        .children(&arena)
        .filter_map(|child| {
            let block = block_node(&arena, child, source, None)?;
            if let Some(pos) = arena[child].pos() {
                root_starts.push(pos);
            } else {
                positions_available = false;
            }
            Some(block)
        })
        .collect();
    ParsedDocument {
        root: BlockNode::Root { children },
        root_starts: positions_available.then_some(root_starts),
    }
}

fn block_node(
    arena: &Arena,
    node_ref: NodeRef,
    source: &str,
    list_spread: Option<bool>,
) -> Option<BlockNode> {
    let node = &arena[node_ref];
    let block = match node.kind_data() {
        KindData::Document(_) => BlockNode::Root {
            children: block_children(arena, node_ref, source, None),
        },
        KindData::Paragraph(_) => {
            let paragraph = inline_paragraph(arena, node_ref, source);
            if paragraph.children.is_empty() {
                return None;
            }
            split_math_blocks(paragraph)
        }
        KindData::Extension(extension)
            if extension.as_any().is::<super::math_parse::MathBlock>() =>
        {
            let paragraph = inline_paragraph(arena, node_ref, source);
            if paragraph.children.is_empty() {
                return None;
            }
            split_math_blocks(paragraph)
        }
        KindData::Heading(heading) => BlockNode::Heading {
            level: heading.level(),
            children: inline_paragraph(arena, node_ref, source),
        },
        KindData::ThematicBreak(_) => BlockNode::HorizontalRule,
        KindData::CodeBlock(code) => {
            let value = code.value().iter(source).collect::<String>();
            let lang = match code.code_block_kind() {
                CodeBlockKind::Fenced => code
                    .language_str(source)
                    .filter(|lang| !lang.is_empty())
                    .map(Into::into),
                CodeBlockKind::Indented => None,
                _ => None,
            };
            BlockNode::CodeBlock(CodeBlock {
                code: value.into(),
                lang,
                ..Default::default()
            })
        }
        KindData::Blockquote(_) => BlockNode::Blockquote {
            children: block_children(arena, node_ref, source, None),
        },
        KindData::List(list) => {
            let spread = !list.is_tight();
            BlockNode::List {
                children: block_children(arena, node_ref, source, Some(spread)),
                ordered: list.is_ordered(),
                start: list.start(),
            }
        }
        KindData::ListItem(item) => BlockNode::ListItem {
            children: block_children(arena, node_ref, source, None),
            spread: list_spread.unwrap_or(false),
            checked: item.task().map(|task| matches!(task, Task::Completed)),
        },
        KindData::HtmlBlock(html) => {
            let text = html.value().iter(source).collect::<String>();
            BlockNode::Paragraph(Paragraph {
                children: vec![plain_inline(text)],
                ..Default::default()
            })
        }
        KindData::LinkReferenceDefinition(_) => return None,
        KindData::Table(_) => table_node(arena, node_ref, source),
        _ => BlockNode::Unknown,
    };
    Some(block)
}

fn split_math_blocks(paragraph: Paragraph) -> BlockNode {
    if !paragraph
        .children
        .iter()
        .any(|node| node.math.as_ref().is_some_and(|math| math.display))
    {
        return BlockNode::Paragraph(paragraph);
    }
    let mut blocks = Vec::new();
    let mut pending = Paragraph::default();
    for node in paragraph.children {
        if let Some(math) = node.math.as_ref().filter(|math| math.display) {
            if !pending.text().trim().is_empty() {
                blocks.push(BlockNode::Paragraph(std::mem::take(&mut pending)));
            } else {
                pending = Paragraph::default();
            }
            blocks.push(BlockNode::CodeBlock(CodeBlock {
                code: math.source.clone().into(),
                lang: Some("latex".into()),
                ..Default::default()
            }));
        } else {
            pending.children.push(node);
        }
    }
    if !pending.text().trim().is_empty() {
        blocks.push(BlockNode::Paragraph(pending));
    }
    BlockNode::Root { children: blocks }
}

fn block_children(
    arena: &Arena,
    parent: NodeRef,
    source: &str,
    list_spread: Option<bool>,
) -> Vec<BlockNode> {
    arena[parent]
        .children(arena)
        .filter_map(|child| block_node(arena, child, source, list_spread))
        .collect()
}

fn table_node(arena: &Arena, table_ref: NodeRef, source: &str) -> BlockNode {
    let mut table = Table::default();
    for section_ref in arena[table_ref].children(arena) {
        match arena[section_ref].kind_data() {
            KindData::TableHeader(_) => {
                for row_ref in arena[section_ref].children(arena) {
                    if matches!(arena[row_ref].kind_data(), KindData::TableRow(_)) {
                        let (row, aligns) = table_row(arena, row_ref, source, true);
                        table.children.push(row);
                        table.column_aligns = aligns;
                    }
                }
            }
            KindData::TableBody(_) => {
                for row_ref in arena[section_ref].children(arena) {
                    if matches!(arena[row_ref].kind_data(), KindData::TableRow(_)) {
                        table
                            .children
                            .push(table_row(arena, row_ref, source, false).0);
                    }
                }
            }
            KindData::TableRow(_) => {
                let (row, aligns) =
                    table_row(arena, section_ref, source, table.children.is_empty());
                if table.children.is_empty() {
                    table.column_aligns = aligns;
                }
                table.children.push(row);
            }
            _ => {}
        }
    }
    BlockNode::Table(table)
}

fn table_row(
    arena: &Arena,
    row_ref: NodeRef,
    source: &str,
    header: bool,
) -> (TableRow, Vec<ColumnumnAlign>) {
    let mut row = TableRow::default();
    let mut aligns = Vec::new();
    for cell_ref in arena[row_ref].children(arena) {
        let KindData::TableCell(cell) = arena[cell_ref].kind_data() else {
            continue;
        };
        if header {
            aligns.push(match cell.alignment() {
                TableCellAlignment::Center => ColumnumnAlign::Center,
                TableCellAlignment::Right => ColumnumnAlign::Right,
                TableCellAlignment::Left | TableCellAlignment::None => ColumnumnAlign::Left,
                _ => ColumnumnAlign::Left,
            });
        }
        row.children.push(TableCell {
            children: inline_paragraph(arena, cell_ref, source),
        });
    }
    (row, aligns)
}

fn inline_paragraph(arena: &Arena, parent: NodeRef, source: &str) -> Paragraph {
    let mut paragraph = Paragraph::default();
    for child in arena[parent].children(arena) {
        parse_inline(&mut paragraph, arena, child, source);
    }
    paragraph
}

fn parse_inline(
    paragraph: &mut Paragraph,
    arena: &Arena,
    node_ref: NodeRef,
    source: &str,
) -> String {
    match arena[node_ref].kind_data() {
        KindData::Extension(extension) => {
            let Some(math) = extension
                .as_any()
                .downcast_ref::<super::math_parse::MathSpan>()
            else {
                return String::new();
            };
            let mut node = plain_inline(math.source.clone());
            node.math = Some(math.clone());
            paragraph.children.push(node);
            math.source.clone()
        }
        KindData::Text(text) => {
            let mut value = decode_entities(text.str(source));
            if text.has_qualifiers(TextQualifier::SOFT_LINE_BREAK)
                || text.has_qualifiers(TextQualifier::HARD_LINE_BREAK)
            {
                value.push('\n');
            }
            paragraph.children.push(plain_inline(value.clone()));
            value
        }
        KindData::CodeSpan(code) => {
            let value = code.str(source).into_owned();
            paragraph
                .children
                .push(marked_inline(value.clone(), TextMark::default().code()));
            value
        }
        KindData::Emphasis(_) => merge_children_with_mark(
            paragraph,
            arena,
            node_ref,
            source,
            TextMark::default().italic(),
        ),
        KindData::Strong(_) => merge_children_with_mark(
            paragraph,
            arena,
            node_ref,
            source,
            TextMark::default().bold(),
        ),
        KindData::Strikethrough(_) => merge_children_with_mark(
            paragraph,
            arena,
            node_ref,
            source,
            TextMark::default().strikethrough(),
        ),
        KindData::Link(link) => merge_children_with_mark(
            paragraph,
            arena,
            node_ref,
            source,
            TextMark::default().link(LinkMark {
                url: link.destination_str(source).into(),
                title: link
                    .title_str(source)
                    .map(|title| title.into_owned().into()),
            }),
        ),
        KindData::Image(image) => {
            let alt = descendant_text(arena, node_ref, source);
            paragraph.children.push(InlineNode::image(ImageNode {
                url: image.destination_str(source).to_string().into(),
                title: image
                    .title_str(source)
                    .map(|title| title.into_owned().into()),
                alt: Some(alt.into()),
                ..Default::default()
            }));
            String::new()
        }
        KindData::RawHtml(html) => {
            let raw = html.str(source).into_owned();
            let value = match raw.trim().to_ascii_lowercase().as_str() {
                "<br>" | "<br/>" | "<br />" => "\n".to_string(),
                _ => raw,
            };
            paragraph.children.push(plain_inline(value.clone()));
            value
        }
        _ => String::new(),
    }
}

fn merge_children_with_mark(
    paragraph: &mut Paragraph,
    arena: &Arena,
    parent: NodeRef,
    source: &str,
    mark: TextMark,
) -> String {
    let mut text = String::new();
    let mut merged_text = String::new();
    let mut merged_marks = Vec::new();

    for child in arena[parent].children(arena) {
        let mut child_paragraph = Paragraph::default();
        text.push_str(&parse_inline(&mut child_paragraph, arena, child, source));
        for node in child_paragraph.children {
            if node.math.is_some() {
                push_merged(paragraph, &mut merged_text, &mut merged_marks, mark.clone());
                paragraph.children.push(node);
                continue;
            }
            let offset = merged_text.len();
            merged_text.push_str(&node.text);
            merged_marks.extend(
                node.marks.into_iter().map(|(range, child_mark)| {
                    (range.start + offset..range.end + offset, child_mark)
                }),
            );
            if let Some(mut image) = node.image {
                if let Some(link) = mark.link.clone() {
                    image.link = Some(link);
                }
                push_merged(paragraph, &mut merged_text, &mut merged_marks, mark.clone());
                paragraph.children.push(InlineNode::image(image));
            }
        }
    }
    push_merged(paragraph, &mut merged_text, &mut merged_marks, mark);
    text
}

fn push_merged(
    paragraph: &mut Paragraph,
    text: &mut String,
    marks: &mut Vec<(std::ops::Range<usize>, TextMark)>,
    mark: TextMark,
) {
    if text.is_empty() {
        return;
    }
    let value = std::mem::take(text);
    let mut node = InlineNode::new(value).marks(std::mem::take(marks));
    let len = node.text.len();
    if let Some((range, last_mark)) = node.marks.last_mut()
        && range.start == 0
        && range.end == len
    {
        last_mark.merge(mark);
    } else {
        node.marks.push((0..len, mark));
    }
    paragraph.children.push(node);
}

fn plain_inline(text: impl Into<String>) -> InlineNode {
    let text = text.into();
    let len = text.len();
    InlineNode::new(text).marks(vec![(0..len, TextMark::default())])
}

fn marked_inline(text: String, mark: TextMark) -> InlineNode {
    let len = text.len();
    InlineNode::new(text).marks(vec![(0..len, mark)])
}

fn descendant_text(arena: &Arena, parent: NodeRef, source: &str) -> String {
    let mut result = String::new();
    for child in arena[parent].children(arena) {
        match arena[child].kind_data() {
            KindData::Text(text) => result.push_str(&decode_entities(text.str(source))),
            KindData::CodeSpan(code) => result.push_str(&code.str(source)),
            _ => result.push_str(&descendant_text(arena, child, source)),
        }
    }
    result
}

/// Decode backslash escapes and entity references in one left-to-right pass,
/// mirroring rushdown's HTML renderer (src/renderer/html.rs): a `\` before
/// ASCII punctuation emits the punctuation literally (per CommonMark; `\`
/// elsewhere is kept), and the emitted character is final — it is NOT
/// re-scanned, so `\&amp;` renders as `&amp;`, not `&`. Unescaped entity
/// references (`&#32;`, `&#x20;`, `&amp;`) decode normally.
fn decode_entities(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() && bytes[i + 1].is_ascii_punctuation() => {
                out.push(bytes[i + 1] as char);
                i += 2;
            }
            b'&' => match decode_entity_at(&bytes[i..]) {
                Some((decoded, consumed)) => {
                    out.push_str(&decoded);
                    i += consumed;
                }
                None => {
                    out.push('&');
                    i += 1;
                }
            },
            _ => {
                let len = utf8_len(bytes[i]);
                out.push_str(&text[i..i + len]);
                i += len;
            }
        }
    }
    out
}

/// Resolve a single entity reference at the start of `bytes` (`&` included).
/// Returns the decoded text and the reference's byte length, or `None` when
/// the text is not a valid reference.
fn decode_entity_at(bytes: &[u8]) -> Option<(String, usize)> {
    // Longest named reference is ~32 chars; cap the scan.
    let end = bytes
        .iter()
        .take(35)
        .position(|b| *b == b';')?
        .checked_add(1)?;
    if end < 3 {
        return None;
    }
    let fragment = &bytes[..end];
    // A valid reference resolves to different (shorter) bytes; an invalid
    // fragment is returned unchanged by the resolvers.
    let numeric = rushdown::util::resolve_numeric_references(Cow::Borrowed(fragment));
    if numeric.as_ref() != fragment {
        return Some((String::from_utf8_lossy(&numeric).into_owned(), end));
    }
    let named = rushdown::util::resolve_entity_references(Cow::Borrowed(fragment));
    if named.as_ref() != fragment {
        return Some((String::from_utf8_lossy(&named).into_owned(), end));
    }
    None
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_children(source: &str) -> Vec<BlockNode> {
        let BlockNode::Root { children, .. } = parse(source) else {
            panic!("expected root");
        };
        children
    }

    fn paragraph(source: &str) -> Paragraph {
        let mut children = root_children(source);
        let BlockNode::Paragraph(paragraph) = children.remove(0) else {
            panic!("expected paragraph");
        };
        paragraph
    }

    #[test]
    fn math_delimiters_preserve_tex_inside_markdown() {
        for source in [
            r"before \(x_i^2+\frac{1}{2}\) after",
            r"before $x_i^2+\frac{1}{2}$ after",
            r"before **\(x_i^2+\frac{1}{2}\)** after",
        ] {
            let p = paragraph(source);
            let formulas: Vec<_> = p
                .children
                .iter()
                .filter_map(|node| node.math.as_ref())
                .collect();
            assert_eq!(formulas.len(), 1, "{source}");
            assert_eq!(formulas[0].source, r"x_i^2+\frac{1}{2}");
            assert_eq!(p.text(), r"before x_i^2+\frac{1}{2} after");
        }
        for source in [
            r"Cost $5 and $10.",
            r"`$x$` and `\(y\)`",
            r"\$x\$",
            r"unfinished \(x_i",
        ] {
            assert!(
                paragraph(source)
                    .children
                    .iter()
                    .all(|node| node.math.is_none()),
                "{source}"
            );
        }
        let source = r"1. Hessian

   \[
   H=\begin{pmatrix}
   x&-y\\
   -y&-x
   \end{pmatrix}
   \]

   Next paragraph.";
        let document = parse(source);
        assert!(
            document.text().contains(
                r"H=\begin{pmatrix}
x&-y\\
-y&-x
\end{pmatrix}"
            ),
            "{}",
            document.text()
        );
        assert!(document.text().contains("Next paragraph."));
    }

    #[test]
    fn display_math_lines_are_not_paragraph_structure() {
        let source = "Differentiating gives\n\\[\n\\frac{d}{dt}K(t)\n=\n-C t^q\n\\tag{30}\n\\]\nafter reducing.\n\n$$\nx = y\n---\n$$\n";
        let blocks = root_children(source);
        let latex: Vec<_> = blocks
            .iter()
            .filter_map(|block| match block {
                BlockNode::Root { children } => Some(children),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                BlockNode::CodeBlock(code) if code.lang.as_deref() == Some("latex") => {
                    Some(code.code.to_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            latex,
            [
                "\n\\frac{d}{dt}K(t)\n=\n-C t^q\n\\tag{30}\n",
                "\nx = y\n---\n"
            ],
            "{blocks:?}"
        );
        assert!(
            !blocks.iter().any(|block| matches!(
                block,
                BlockNode::Heading { .. } | BlockNode::HorizontalRule
            )),
            "{blocks:?}"
        );
        let text = parse(source).text();
        assert!(text.contains("Differentiating gives"), "{text}");
        assert!(text.contains("after reducing."), "{text}");

        let text = parse("\\[ x \\] after $$ y $$ end").text();
        assert!(text.contains("after") && text.contains("end"), "{text}");
    }

    #[test]
    fn inline_mapping_preserves_combined_marks_and_link_metadata() {
        let p = paragraph("a **bold** *italic* ~~strike~~ `code` [label](https://url \"title\")");
        assert_eq!(p.text(), "a bold italic strike code label");
        let marked = p
            .children
            .iter()
            .filter(|node| node.marks[0].1 != TextMark::default())
            .collect::<Vec<_>>();
        assert_eq!(marked.len(), 5);
        assert!(marked[0].marks[0].1.bold);
        assert!(marked[1].marks[0].1.italic);
        assert!(marked[2].marks[0].1.strikethrough);
        assert!(marked[3].marks[0].1.code);
        assert_eq!(
            marked[4].marks[0].1.link.as_ref().unwrap(),
            &LinkMark {
                url: "https://url".into(),
                title: Some("title".into())
            }
        );
        assert!(
            marked
                .iter()
                .all(|node| node.marks[0].0 == (0..node.text.len()))
        );
        let p = paragraph("***both***");
        assert_eq!(p.children.len(), 1);
        assert_eq!(p.children[0].text.as_ref(), "both");
        assert!(
            p.children[0]
                .marks
                .iter()
                .any(|(_, mark)| mark.bold && mark.italic)
        );
    }

    #[test]
    fn inline_text_preserves_breaks_html_and_escaped_entities() {
        assert_eq!(paragraph("a\nb").text(), "a\nb");
        assert_eq!(paragraph("a  \nb").text(), "a\nb");
        assert_eq!(paragraph("a<br>b").text(), "a\nb");
        assert_eq!(paragraph("&#32;x\n&#9;y &amp; z").text(), " x\n\ty & z");
        // Escaped punctuation loses its backslash (CommonMark).
        assert_eq!(paragraph(r"\*not italic\*").text(), "*not italic*");
        assert_eq!(paragraph(r"\\").text(), r"\");
        // `\` before a non-punctuation char is kept literally.
        assert_eq!(paragraph(r"\a").text(), r"\a");
        // An escaped `&` does NOT start an entity reference: the emitted
        // character is final, so `\&amp;` renders literally.
        assert_eq!(paragraph(r"\&amp\;").text(), "&amp;");
        // Unescaped entities still decode alongside escapes.
        assert_eq!(paragraph(r"\*&#32;\*").text(), "* *");
        assert_eq!(paragraph("a<span>b</span>c").text(), "a<span>b</span>c");
    }

    #[test]
    fn list_mapping_preserves_tasks_spacing_and_numbering() {
        let children = root_children("- [x] done\n- [ ] todo\n- plain");
        let BlockNode::List {
            children: items,
            ordered,
            ..
        } = &children[0]
        else {
            panic!()
        };
        assert!(!ordered);
        let checked = items
            .iter()
            .map(|item| match item {
                BlockNode::ListItem {
                    checked, spread, ..
                } => (*checked, *spread),
                _ => panic!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            checked,
            vec![(Some(true), false), (Some(false), false), (None, false)]
        );

        let children = root_children("1. one\n\n2. two");
        let BlockNode::List {
            children: items,
            ordered,
            ..
        } = &children[0]
        else {
            panic!()
        };
        assert!(ordered);
        assert!(
            items
                .iter()
                .all(|item| matches!(item, BlockNode::ListItem { spread: true, .. }))
        );
        let children = root_children("1. one\n\ntext\n\n2. two\n3. three");
        let BlockNode::List { ordered, start, .. } = &children[2] else {
            panic!()
        };
        assert!(ordered);
        assert_eq!(*start, 2);
    }

    #[test]
    fn maps_table_header_rows_and_alignment() {
        let children = root_children("| a | b |\n| :---: | ---: |\n| c | d |");
        let BlockNode::Table(table) = &children[0] else {
            panic!()
        };
        assert_eq!(
            table.column_aligns,
            vec![ColumnumnAlign::Center, ColumnumnAlign::Right]
        );
        assert_eq!(table.children.len(), 2);
        assert_eq!(table.children[0].children[0].children.text(), "a");
    }

    #[test]
    fn block_mapping_preserves_code_and_nested_document_structure() {
        let children = root_children("```rust extra\nfn main() {}\n```\n\n    indented\n");
        let BlockNode::CodeBlock(fenced) = &children[0] else {
            panic!()
        };
        assert_eq!(fenced.lang.as_deref(), Some("rust"));
        assert_eq!(fenced.code.as_ref(), "fn main() {}\n");
        let BlockNode::CodeBlock(indented) = &children[1] else {
            panic!()
        };
        assert_eq!(indented.lang, None);
        assert_eq!(indented.code.as_ref(), "indented\n");
        let children = root_children("> quote\n>\n> - item");
        let BlockNode::Blockquote { children, .. } = &children[0] else {
            panic!()
        };
        assert!(matches!(
            children.as_slice(),
            [BlockNode::Paragraph(_), BlockNode::List { .. }]
        ));
        let children = root_children("# 1\n## 2\n### 3\n#### 4\n##### 5\n###### 6");
        assert_eq!(
            children
                .iter()
                .map(|node| match node {
                    BlockNode::Heading { level, .. } => *level,
                    _ => panic!(),
                })
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert!(matches!(
            root_children("---").as_slice(),
            [BlockNode::HorizontalRule]
        ));
    }

    #[test]
    fn link_mapping_preserves_destinations_titles_and_image_alternatives() {
        let p = paragraph("![alt](url \"title\")");
        let image = p.children[0].image.as_ref().unwrap();
        assert_eq!(image.url.as_ref(), "url");
        assert_eq!(image.alt.as_deref(), Some("alt"));
        assert_eq!(image.title(), "title");
        for source in ["<https://x.com>", "https://x.com"] {
            let p = paragraph(source);
            assert_eq!(p.text(), "https://x.com");
            assert_eq!(
                p.children[0].marks[0].1.link.as_ref().unwrap().url.as_ref(),
                "https://x.com"
            );
        }
        let children = root_children("[a][b]\n\n[b]: https://u \"title\"");
        assert_eq!(children.len(), 1, "definition nodes are skipped");
        let BlockNode::Paragraph(p) = &children[0] else {
            panic!()
        };
        let link = p.children[0].marks[0].1.link.as_ref().unwrap();
        assert_eq!(link.url.as_ref(), "https://u");
        assert_eq!(link.title.as_deref(), Some("title"));
    }
}
