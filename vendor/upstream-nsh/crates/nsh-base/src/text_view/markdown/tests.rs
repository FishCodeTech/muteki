//! Tests of the Markdown parser.

use std::time::Instant;

use super::super::{
    Alert, Block, CodeBlock, ColumnAlign, CustomNode, Document, Footnote, Image, Inline, ListItem,
    MarkdownExtension, MarkdownOptions, MarkdownPlugin, ParseContext, Parser, Table, TableCell,
    mdast,
};

fn md(source: &str) -> Document {
    Parser::new().markdown(source)
}

fn blocks(source: &str) -> Vec<Block> {
    md(source).blocks
}

fn t(text: &str) -> Inline {
    Inline::text(text)
}

fn p(content: Vec<Inline>) -> Block {
    Block::paragraph(content)
}

fn strong(content: Vec<Inline>) -> Inline {
    Inline::Strong { content }
}

fn em(content: Vec<Inline>) -> Inline {
    Inline::Emphasis { content }
}

fn code(code: &str) -> Inline {
    Inline::Code { code: code.into() }
}

fn link(url: &str, content: Vec<Inline>) -> Inline {
    Inline::Link {
        url: url.into(),
        title: None,
        content,
    }
}

fn heading(level: u8, text: &str, anchor: &str) -> Block {
    Block::Heading {
        level,
        content: vec![t(text)],
        anchor: Some(anchor.into()),
    }
}

fn item(children: Vec<Block>) -> ListItem {
    ListItem::new(children)
}

fn options() -> MarkdownOptions {
    MarkdownOptions::default()
}

#[test]
fn paragraphs_and_emphasis() {
    assert_eq!(
        blocks("Hello **bold** and *it* and ~~gone~~ and `code`.\n\nSecond"),
        vec![
            p(vec![
                t("Hello "),
                strong(vec![t("bold")]),
                t(" and "),
                em(vec![t("it")]),
                t(" and "),
                Inline::Strikethrough {
                    content: vec![t("gone")]
                },
                t(" and "),
                code("code"),
                t("."),
            ]),
            p(vec![t("Second")]),
        ]
    );
}

#[test]
fn soft_and_hard_breaks() {
    assert_eq!(
        blocks("one\ntwo  \nthree\\\nfour"),
        vec![p(vec![
            t("one two"),
            Inline::Break,
            t("three"),
            Inline::Break,
            t("four"),
        ])]
    );
    // CRLF reads as LF.
    assert_eq!(blocks("a\r\nb\r\n\r\nc"), blocks("a\nb\n\nc"));
}

#[test]
fn headings_and_anchors() {
    assert_eq!(
        blocks("# Intro\n\nIntro\n=====\n\n## Intro\n\nSub\n---\n\n###### Getting *started*!"),
        vec![
            heading(1, "Intro", "intro"),
            heading(1, "Intro", "intro-1"),
            heading(2, "Intro", "intro-2"),
            heading(2, "Sub", "sub"),
            Block::Heading {
                level: 6,
                content: vec![t("Getting "), em(vec![t("started")]), t("!")],
                anchor: Some("getting-started".into()),
            },
        ]
    );
}

#[test]
fn links_images_and_references() {
    let source = "[a](https://a.com \"T\") [b][ref] [ref] ![i](i.png) ![r][img] <https://auto.dev> www.x.com x@y.com [none]\n\n[ref]: https://ref.dev\n[img]: r.png \"R\"";
    assert_eq!(
        blocks(source),
        vec![p(vec![
            Inline::Link {
                url: "https://a.com".into(),
                title: Some("T".into()),
                content: vec![t("a")],
            },
            t(" "),
            link("https://ref.dev", vec![t("b")]),
            t(" "),
            link("https://ref.dev", vec![t("ref")]),
            t(" "),
            Inline::Image(Image {
                src: "i.png".into(),
                alt: "i".into(),
                title: None,
                ..Image::default()
            }),
            t(" "),
            Inline::Image(Image {
                src: "r.png".into(),
                alt: "r".into(),
                title: Some("R".into()),
                ..Image::default()
            }),
            t(" "),
            link("https://auto.dev", vec![t("https://auto.dev")]),
            t(" "),
            link("http://www.x.com", vec![t("www.x.com")]),
            t(" "),
            link("mailto:x@y.com", vec![t("x@y.com")]),
            t(" [none]"),
        ])]
    );
}

#[test]
fn entities_are_decoded() {
    assert_eq!(
        blocks("a &amp; b &copy; &#65;"),
        vec![p(vec![t("a & b © A")])]
    );
}

#[test]
fn quotes_and_alerts() {
    assert_eq!(
        blocks("> quoted\n> > nested"),
        vec![Block::Blockquote {
            children: vec![
                p(vec![t("quoted")]),
                Block::Blockquote {
                    children: vec![p(vec![t("nested")])],
                    alert: None,
                },
            ],
            alert: None,
        }]
    );
    assert_eq!(
        blocks("> [!NOTE]\n> Useful **info**.\n\n> [!warning]\n\n> [!TIP]\n> \n> Para"),
        vec![
            Block::Blockquote {
                children: vec![p(vec![t("Useful "), strong(vec![t("info")]), t(".")])],
                alert: Some(Alert::Note),
            },
            Block::Blockquote {
                children: vec![],
                alert: Some(Alert::Warning),
            },
            Block::Blockquote {
                children: vec![p(vec![t("Para")])],
                alert: Some(Alert::Tip),
            },
        ]
    );
    // Not alone on its line, unknown, or turned off: a quote.
    for (source, options) in [
        ("> [!NOTE] inline", options()),
        ("> [!OTHER]\n> x", options()),
        (
            "> [!NOTE]\n> x",
            MarkdownOptions {
                alerts: false,
                ..options()
            },
        ),
    ] {
        let doc = Parser::new().options(options).markdown(source);
        assert!(
            matches!(&doc.blocks[0], Block::Blockquote { alert: None, .. }),
            "{source}: {doc:?}"
        );
    }
}

#[test]
fn lists() {
    assert_eq!(
        blocks("- a\n- b\n  1. c\n\n3. x\n4. y\n\n   z\n\n- [ ] todo\n- [x] done"),
        vec![
            Block::List {
                ordered: false,
                start: 1,
                loose: false,
                items: vec![
                    item(vec![p(vec![t("a")])]),
                    item(vec![
                        p(vec![t("b")]),
                        Block::List {
                            ordered: true,
                            start: 1,
                            loose: false,
                            items: vec![item(vec![p(vec![t("c")])])],
                        },
                    ]),
                ],
            },
            Block::List {
                ordered: true,
                start: 3,
                loose: true,
                items: vec![
                    item(vec![p(vec![t("x")])]),
                    item(vec![p(vec![t("y")]), p(vec![t("z")])]),
                ],
            },
            Block::List {
                ordered: false,
                start: 1,
                loose: false,
                items: vec![
                    ListItem {
                        checked: Some(false),
                        children: vec![p(vec![t("todo")])],
                    },
                    ListItem {
                        checked: Some(true),
                        children: vec![p(vec![t("done")])],
                    },
                ],
            },
        ]
    );
}

#[test]
fn code_blocks_and_rules() {
    assert_eq!(
        blocks(
            "```rust title=\"main.rs\"\nfn main() {}\n```\n\n    indented\n\n***\n\n~~~\nplain\n~~~"
        ),
        vec![
            Block::CodeBlock(CodeBlock {
                language: Some("rust".into()),
                meta: Some("title=\"main.rs\"".into()),
                code: "fn main() {}".into(),
            }),
            Block::CodeBlock(CodeBlock::new(None, "indented")),
            Block::Rule,
            Block::CodeBlock(CodeBlock::new(None, "plain")),
        ]
    );
    // An unclosed fence runs to the end.
    assert_eq!(
        blocks("```js\nlet a"),
        vec![Block::CodeBlock(CodeBlock::new(Some("js".into()), "let a"))]
    );
}

#[test]
fn tables() {
    assert_eq!(
        blocks("| a | b | c |\n|:--|:-:|--:|\n| 1 | **2** |\n| x | y | z | extra |"),
        vec![Block::Table(Table {
            align: vec![ColumnAlign::Left, ColumnAlign::Center, ColumnAlign::Right],
            head: vec![
                TableCell::new(vec![t("a")]),
                TableCell::new(vec![t("b")]),
                TableCell::new(vec![t("c")]),
            ],
            rows: vec![
                vec![
                    TableCell::new(vec![t("1")]),
                    TableCell::new(vec![strong(vec![t("2")])]),
                    TableCell::default(),
                ],
                vec![
                    TableCell::new(vec![t("x")]),
                    TableCell::new(vec![t("y")]),
                    TableCell::new(vec![t("z")]),
                ],
            ],
        })]
    );
}

#[test]
fn footnotes_numbered_by_first_reference() {
    let doc = md(
        "B[^b] A[^a] B again[^b]\n\n[^a]: Note A[^c]\n[^b]: Note B\n[^c]: Note C\n[^unused]: Never",
    );
    assert_eq!(
        doc.blocks,
        vec![p(vec![
            t("B"),
            Inline::FootnoteReference {
                label: "b".into(),
                index: 1
            },
            t(" A"),
            Inline::FootnoteReference {
                label: "a".into(),
                index: 2
            },
            t(" B again"),
            Inline::FootnoteReference {
                label: "b".into(),
                index: 1
            },
        ])]
    );
    assert_eq!(
        doc.footnotes,
        vec![
            Footnote {
                label: "b".into(),
                index: 1,
                children: vec![p(vec![t("Note B")])],
            },
            Footnote {
                label: "a".into(),
                index: 2,
                children: vec![p(vec![
                    t("Note A"),
                    Inline::FootnoteReference {
                        label: "c".into(),
                        index: 3
                    }
                ])],
            },
            Footnote {
                label: "c".into(),
                index: 3,
                children: vec![p(vec![t("Note C")])],
            },
        ]
    );
}

#[test]
fn math() {
    assert_eq!(
        blocks(
            "Euler: $e^{i\\pi}+1=0$ and $$x$$.\n\n$$\n\\int_0^1 x\\,dx\n$$\n\n```math\na^2\n```"
        ),
        vec![
            p(vec![
                t("Euler: "),
                Inline::Math {
                    tex: "e^{i\\pi}+1=0".into()
                },
                t(" and "),
                Inline::Math { tex: "x".into() },
                t("."),
            ]),
            Block::Math {
                tex: "\\int_0^1 x\\,dx".into()
            },
            Block::Math { tex: "a^2".into() },
        ]
    );
    // Prices are prose.
    assert_eq!(
        blocks("It costs $5 and $10, or $x$."),
        vec![p(vec![
            t("It costs $5 and $10, or "),
            Inline::Math { tex: "x".into() },
            t("."),
        ])]
    );
    assert_eq!(
        blocks("Between $1 and *$2*"),
        vec![p(vec![t("Between $1 and "), em(vec![t("$2")])])]
    );
    // Turned off: text.
    let doc = Parser::new()
        .options(MarkdownOptions {
            math: false,
            ..options()
        })
        .markdown("$x$\n\n```math\na\n```");
    assert_eq!(
        doc.blocks,
        vec![
            p(vec![t("$x$")]),
            Block::CodeBlock(CodeBlock::new(Some("math".into()), "a"))
        ]
    );
}

#[test]
fn tex_delimiters() {
    assert_eq!(
        blocks(
            "Euler \\(e^{i\\pi} + 1 = 0\\), inline \\[x\\].\n\n\\[\n\\int_0^1 x\\,dx\n\\]\n\nAfter `\\(code\\)`"
        ),
        vec![
            p(vec![
                t("Euler "),
                Inline::Math {
                    tex: "e^{i\\pi} + 1 = 0".into()
                },
                t(", inline "),
                Inline::Math { tex: "x".into() },
                t("."),
            ]),
            Block::Math {
                tex: "\\int_0^1 x\\,dx".into()
            },
            p(vec![t("After "), code("\\(code\\)")]),
        ]
    );
    // In a list item and a quote.
    assert_eq!(
        blocks("> \\[ a^2 \\]"),
        vec![Block::Blockquote {
            children: vec![Block::Math { tex: "a^2".into() }],
            alert: None,
        }]
    );
    // In a fence: code.
    assert_eq!(
        blocks("```\n\\(x\\)\n```"),
        vec![Block::CodeBlock(CodeBlock::new(None, "\\(x\\)"))]
    );
    // Math off: escaped parentheses, as Markdown reads them.
    let doc = Parser::new()
        .options(MarkdownOptions {
            math: false,
            ..options()
        })
        .markdown("\\(x\\)");
    assert_eq!(doc.blocks, vec![p(vec![t("(x)")])]);
    // Streamed: closed while open.
    let doc = Parser::new()
        .options(MarkdownOptions {
            streaming: true,
            ..options()
        })
        .markdown("So \\(x^2 + y");
    assert_eq!(
        doc.blocks,
        vec![p(vec![
            t("So "),
            Inline::Math {
                tex: "x^2 + y".into()
            }
        ])]
    );
}

#[test]
fn frontmatter() {
    let source = "---\ntitle: Hi\n---\n# Body";
    let doc = Parser::new()
        .options(MarkdownOptions {
            frontmatter: true,
            ..options()
        })
        .markdown(source);
    assert_eq!(
        doc.blocks,
        vec![
            Block::Custom(CustomNode::new("frontmatter").with_text("title: Hi")),
            heading(1, "Body", "body"),
        ]
    );
    // Off: a rule and a heading.
    assert_eq!(blocks(source)[0], Block::Rule);
}

#[test]
fn html_in_markdown() {
    let doc = md("Press <kbd>Ctrl</kbd> + <b>x</b>, E=mc<sup>2</sup><br>next");
    let [Block::Paragraph { content }] = doc.blocks.as_slice() else {
        panic!("{doc:?}");
    };
    assert_eq!(content[0], t("Press "));
    assert!(
        content.contains(&Inline::Keyboard {
            text: "Ctrl".into()
        }),
        "{content:?}"
    );
    assert!(content.contains(&strong(vec![t("x")])), "{content:?}");
    assert!(
        content.contains(&Inline::Superscript {
            content: vec![t("2")]
        }),
        "{content:?}"
    );
    assert!(content.contains(&Inline::Break), "{content:?}");

    // Flow HTML with Markdown between its tags.
    let doc = md("<details>\n<summary>More</summary>\n\nHidden **text**\n\n</details>\n\nAfter");
    assert_eq!(
        doc.blocks,
        vec![
            Block::Details {
                open: false,
                summary: vec![t("More")],
                children: vec![p(vec![t("Hidden "), strong(vec![t("text")])])],
            },
            p(vec![t("After")]),
        ]
    );

    // Turned off: text.
    let doc = Parser::new()
        .options(MarkdownOptions {
            html: false,
            ..options()
        })
        .markdown("a <b>x</b>\n\n<div>\nblock\n</div>");
    assert_eq!(
        doc.blocks,
        vec![
            p(vec![t("a <b>x</b>")]),
            p(vec![
                t("<div>"),
                Inline::Break,
                t("block"),
                Inline::Break,
                t("</div>")
            ]),
        ]
    );
}

#[test]
fn mdx() {
    let parser = Parser::new()
        .options(MarkdownOptions {
            mdx: true,
            ..options()
        })
        .extension(MarkdownExtension::tag("callout", "Callout"));
    let doc = parser.markdown(
        "<Callout kind=\"tip\" open n={1 + 1}>\n  Some *text*\n</Callout>\n\nHi <Badge tone='x'>New</Badge> {name}\n\n{flow}\n\n<>\nfrag\n</>",
    );
    assert_eq!(
        doc.blocks,
        vec![
            Block::Custom(CustomNode {
                name: "callout".into(),
                attributes: [("kind", "tip"), ("open", "true"), ("n", "1 + 1")]
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect(),
                text: "Some text".into(),
                children: vec![p(vec![t("Some "), em(vec![t("text")])])],
                ..CustomNode::default()
            }),
            p(vec![
                t("Hi "),
                Inline::Custom(CustomNode {
                    name: "Badge".into(),
                    attributes: [("tone".into(), "x".into())].into_iter().collect(),
                    text: "New".into(),
                    content: vec![t("New")],
                    ..CustomNode::default()
                }),
                t(" "),
                Inline::Custom(CustomNode::new("expression").with_text("name")),
            ]),
            Block::Custom(CustomNode::new("expression").with_text("flow")),
            p(vec![t("frag")]),
        ]
    );
    // JSX not closed yet: read as Markdown.
    let doc = parser.markdown("Text <Callout");
    assert_eq!(doc.blocks.len(), 1);
}

#[test]
fn language_extension() {
    let doc = Parser::new()
        .extension(MarkdownExtension::language("chart", "Chart"))
        .markdown("```chart {\"w\":1}\nbar 1 2\n```\n\n```rust\nx\n```");
    assert_eq!(
        doc.blocks,
        vec![
            Block::Custom(
                CustomNode::new("chart")
                    .with_text("bar 1 2")
                    .with_attribute("meta", "{\"w\":1}")
            ),
            Block::CodeBlock(CodeBlock::new(Some("rust".into()), "x")),
        ]
    );
}

#[test]
fn pattern_extension() {
    let doc = Parser::new()
        .extension(MarkdownExtension::pattern("mention", r"@(\w+)"))
        .markdown("# Hi @ann\n\n**@bo** `@code` [@link](u) | x |\n\n| @c |\n|--|\n| @d |");
    let mention = |name: &str| {
        Inline::Custom(
            CustomNode::new("mention")
                .with_text(format!("@{name}"))
                .with_attribute("1", name),
        )
    };
    assert_eq!(
        doc.blocks,
        vec![
            Block::Heading {
                level: 1,
                content: vec![t("Hi "), mention("ann")],
                anchor: Some("hi-ann".into()),
            },
            p(vec![
                strong(vec![mention("bo")]),
                t(" "),
                code("@code"),
                t(" "),
                link("u", vec![t("@link")]),
                t(" | x |"),
            ]),
            Block::Table(Table {
                align: vec![ColumnAlign::None],
                head: vec![TableCell::new(vec![mention("c")])],
                rows: vec![vec![TableCell::new(vec![mention("d")])]],
            }),
        ]
    );
}

#[test]
fn container_extension() {
    let parser = Parser::new()
        .extension(MarkdownExtension::container("note", "note"))
        .extension(MarkdownExtension::pattern("mention", r"@\w+"));
    let doc = parser.markdown(
        "Intro[^1]\n\n:::note[Heads up]{kind=tip .wide}\nInside **bold** @x\n\n```\n:::\n```\n:::\n\n:::other\nstays\n:::\n\n[^1]: Foot",
    );
    assert_eq!(
        doc.blocks,
        vec![
            p(vec![
                t("Intro"),
                Inline::FootnoteReference {
                    label: "1".into(),
                    index: 1
                }
            ]),
            Block::Custom(CustomNode {
                name: "note".into(),
                attributes: [("label", "Heads up"), ("kind", "tip"), ("class", "wide")]
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect(),
                children: vec![
                    p(vec![
                        t("Inside "),
                        strong(vec![t("bold")]),
                        t(" "),
                        Inline::Custom(CustomNode::new("mention").with_text("@x")),
                    ]),
                    Block::CodeBlock(CodeBlock::new(None, ":::")),
                ],
                ..CustomNode::default()
            }),
            p(vec![t(":::other stays :::")]),
        ]
    );
    assert_eq!(doc.footnotes.len(), 1);
}

/// Inline code starting with `=` is a formula of the plugin's.
struct Formulas;

impl MarkdownPlugin for Formulas {
    fn name(&self) -> &str {
        "formulas"
    }

    fn inline(&self, node: &mdast::Node, cx: &mut ParseContext<'_>) -> Option<Inline> {
        let mdast::Node::InlineCode(code) = node else {
            return None;
        };
        let formula = code.value.strip_prefix('=')?;
        assert!(cx.node_source(node).is_some_and(|s| s.starts_with('`')));
        Some(Inline::Custom(
            CustomNode::new("formula").with_text(formula),
        ))
    }

    fn block(&self, node: &mdast::Node, cx: &mut ParseContext<'_>) -> Option<Block> {
        let mdast::Node::Blockquote(quote) = node else {
            return None;
        };
        // Read the children as the parser would, inside a node of its own.
        let children = cx.blocks(&quote.children);
        Some(Block::Custom(
            CustomNode::new("aside").with_children(children),
        ))
    }
}

#[test]
fn plugins() {
    let doc = Parser::new()
        .plugin(Formulas)
        .markdown("Sum `=A1+B1` and `code`\n\n> quoted `=x`");
    assert_eq!(
        doc.blocks,
        vec![
            p(vec![
                t("Sum "),
                Inline::Custom(CustomNode::new("formula").with_text("A1+B1")),
                t(" and "),
                code("code"),
            ]),
            Block::Custom(CustomNode::new("aside").with_children(vec![p(vec![
                t("quoted "),
                Inline::Custom(CustomNode::new("formula").with_text("x")),
            ])])),
        ]
    );
}

#[test]
fn streaming() {
    let parser = Parser::new().options(MarkdownOptions {
        streaming: true,
        ..options()
    });
    assert_eq!(
        parser.markdown("Some **bold").blocks,
        vec![p(vec![t("Some "), strong(vec![t("bold")])])]
    );
    assert_eq!(
        parser.markdown("See [docs](https://exa").blocks,
        vec![p(vec![t("See docs")])]
    );
    assert_eq!(
        parser.markdown("Run `cargo").blocks,
        vec![p(vec![t("Run "), code("cargo")])]
    );
    assert_eq!(
        parser.markdown("| a | b |\n| -").blocks,
        vec![Block::Table(Table {
            align: vec![ColumnAlign::None, ColumnAlign::None],
            head: vec![TableCell::new(vec![t("a")]), TableCell::new(vec![t("b")])],
            rows: vec![],
        })]
    );
    // Without the option, as written.
    assert_eq!(md("Some **bold").blocks, vec![p(vec![t("Some **bold")])]);
}

#[test]
fn empty_paragraphs_are_dropped() {
    assert_eq!(
        blocks("<!-- comment -->\n\na <!-- c --> b"),
        vec![p(vec![t("a  b")])]
    );
    assert_eq!(blocks(""), vec![]);
    assert_eq!(blocks("   \n\n  "), vec![]);
}

#[test]
fn never_panics() {
    let deep_list: String = (0..200)
        .map(|i| format!("{}- x\n", "  ".repeat(i)))
        .collect();
    let deep_quote = format!("{} x", ">".repeat(500));
    let parser = Parser::new()
        .options(MarkdownOptions {
            frontmatter: true,
            streaming: true,
            ..options()
        })
        .extension(MarkdownExtension::container("box", "box"))
        .extension(MarkdownExtension::pattern("any", r"\b"))
        .extension(MarkdownExtension::pattern("bad", r"("));
    let mdx = Parser::new().options(MarkdownOptions {
        mdx: true,
        streaming: true,
        ..options()
    });
    for source in [
        "",
        "***",
        "---",
        "\n",
        "\r\n\r\n",
        "[",
        "![",
        "<",
        "$",
        "$$",
        "`",
        "|",
        "| a |\n|",
        "> [!NOTE]",
        ":::box",
        ":::box\n:::",
        "::::::",
        "[^1]",
        "[^1]:",
        "<details>",
        "</details>",
        "{",
        "<A",
        "中文 **粗体** 😀 *é*",
        "a\u{0}b",
        &deep_list,
        &deep_quote,
    ] {
        let _ = parser.markdown(source);
        let _ = mdx.markdown(source);
        let _ = md(source);
    }
}

#[test]
fn a_long_reply_parses_quickly() {
    let chunk = "## Section\n\nSome **bold** text with `code`, a [link](https://example.com) and $x^2$.\n\n- item one\n- item two with *emphasis*\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> [!NOTE]\n> A note.\n\n";
    let source = chunk.repeat(20_000 / chunk.len() + 1);
    let parser = Parser::new()
        .options(MarkdownOptions {
            streaming: true,
            ..options()
        })
        .extension(MarkdownExtension::pattern("mention", r"@\w+"));
    let start = Instant::now();
    let doc = parser.markdown(&source);
    let elapsed = start.elapsed();
    assert!(doc.blocks.len() > 100);
    // Generous for debug builds; a release build takes a few milliseconds.
    assert!(elapsed.as_secs_f64() < 2.0, "{elapsed:?}");
}

#[test]
fn inline_html_images_keep_their_size_in_the_paragraph() {
    use nsh_core::geometry::Points;

    use super::super::ImageLength;
    // gpui-kit's `test_inline_html_image_stays_in_markdown_paragraph`.
    assert_eq!(
        blocks(
            r#"Before <img src="https://example.com/avatar.png" alt="Avatar" width="32" height="32" /> after."#
        ),
        vec![p(vec![
            t("Before "),
            Inline::Image(Image {
                alt: "Avatar".into(),
                width: Some(ImageLength::Points(Points(32.0))),
                height: Some(ImageLength::Points(Points(32.0))),
                ..Image::new("https://example.com/avatar.png")
            }),
            t(" after."),
        ])]
    );
    // `..._without_size_...`: no size is none.
    assert_eq!(
        blocks(r#"Before <img src="https://example.com/avatar.png" alt="Avatar"> after."#),
        vec![p(vec![
            t("Before "),
            Inline::Image(Image {
                alt: "Avatar".into(),
                ..Image::new("https://example.com/avatar.png")
            }),
            t(" after."),
        ])]
    );
    // A percentage, in an HTML block of its own.
    assert_eq!(
        blocks(r#"<img src="a.png" alt="A" width="100%" />"#),
        vec![p(vec![Inline::Image(Image {
            alt: "A".into(),
            width: Some(ImageLength::Fraction(nsh_core::geometry::Percentage(1.0))),
            ..Image::new("a.png")
        })])]
    );
}

#[test]
fn linked_images_are_in_their_link() {
    let image = Inline::Image(Image {
        alt: "alt".into(),
        ..Image::new("i.png")
    });
    assert_eq!(
        blocks("[![alt](i.png)](https://x.y)"),
        vec![p(vec![link("https://x.y", vec![image.clone()])])]
    );
    assert_eq!(
        blocks(r#"<a href="https://x.y"><img src="i.png" alt="alt"></a>"#),
        vec![p(vec![link("https://x.y", vec![image])])]
    );
}

#[test]
fn inline_html_marks_keep_their_color() {
    // gpui-kit's `inline_html_mark_applies_highlight_colors`.
    assert_eq!(
        blocks(
            r#"a <mark color="yellow">b</mark> <mark style="background-color: #336699">c</mark>"#
        ),
        vec![p(vec![
            t("a "),
            Inline::Highlight {
                content: vec![t("b")],
                color: Some("#facc15".into()),
            },
            t(" "),
            Inline::Highlight {
                content: vec![t("c")],
                color: Some("#336699".into()),
            },
        ])]
    );
}

#[test]
fn inline_html_marks_follow_gpui_kits_colors() {
    // gpui-kit's `inline_html_mark_applies_highlight_colors`.
    let source = concat!(
        r#"Plain <mark>highlighted</mark> text and "#,
        r##"<mark data_id="1" color="#ff000059">red</mark> and "##,
        r#"<mark style="background-color: #336699">hex</mark>."#
    );
    let mark = |text: &str, color: Option<&str>| Inline::Highlight {
        content: vec![t(text)],
        color: color.map(str::to_owned),
    };
    assert_eq!(
        blocks(source),
        vec![p(vec![
            t("Plain "),
            mark("highlighted", None),
            t(" text and "),
            mark("red", Some("#ff000059")),
            t(" and "),
            mark("hex", Some("#336699")),
            t("."),
        ])]
    );
}

#[test]
fn inline_html_formatting_holds_markdown() {
    // gpui-kit's `inline_html_formatting_tags_pair_across_siblings`: the
    // Markdown between paired tags is read as Markdown.
    let doc = blocks(
        "a <strong>b *c* <em>d</em></strong> e <b>f</b> <i>g</i> <del>h</del> <br> <strong>unclosed",
    );
    assert_eq!(
        doc,
        vec![p(vec![
            t("a "),
            strong(vec![
                t("b "),
                Inline::Emphasis {
                    content: vec![t("c")]
                },
                t(" "),
                Inline::Emphasis {
                    content: vec![t("d")]
                },
            ]),
            t(" e "),
            strong(vec![t("f")]),
            t(" "),
            Inline::Emphasis {
                content: vec![t("g")]
            },
            t(" "),
            Inline::Strikethrough {
                content: vec![t("h")]
            },
            t(" "),
            Inline::Break,
            t(" unclosed"),
        ])]
    );
    // A link's content as well; a `<mark>` keeps its color around it.
    assert_eq!(
        blocks(r#"<a href="https://x.y" title="T">**go**</a> <mark color="blue">`x`</mark>"#),
        vec![p(vec![
            Inline::Link {
                url: "https://x.y".into(),
                title: Some("T".into()),
                content: vec![strong(vec![t("go")])],
            },
            t(" "),
            Inline::Highlight {
                content: vec![Inline::Code { code: "x".into() }],
                color: Some("#3b82f6".into()),
            },
        ])]
    );
    // Elements with literal content stay HTML.
    assert_eq!(
        blocks("<code>*x*</code>"),
        vec![p(vec![Inline::Code { code: "*x*".into() }])]
    );
}
