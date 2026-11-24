pub const PREVIEW_BROWSER_INSTRUCTIONS: &str = r#"## tcode preview browser

You are running inside tcode. The `tcode_preview` MCP server is the embedded preview browser shared with the user. When it exposes `preview_*` tools, prefer those tools for browser navigation, inspection, interaction, screenshots, and recordings.

For browser work, first call `preview_status`. If no automation-capable preview is attached, call `preview_open` before concluding that the browser is unavailable. Then use `preview_navigate`, `preview_snapshot`, and the focused interaction tools. Prefer snapshot-provided locators over coordinates.

Do not switch to global browser skills, Chrome, Node REPL browser automation, standalone Playwright, or agent-browser merely because the preview is initially closed or a first call fails. Use an alternative browser system only when the tcode preview tools are absent, the user explicitly requests another browser, or `preview_open` returns an explicit unsupported/unavailable error. A failed tcode preview tool call should be inspected and retried with corrected arguments when the error is actionable."#;
