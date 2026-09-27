# AI Native React Components (Muteki Adaptations)

This directory contains adapted React components derived from [ai-native-react-components](https://github.com/TurboKach/ai-native-react-components).

## Upstream Tracking

- **Upstream Repository**: `https://github.com/TurboKach/ai-native-react-components`
- **Pinned Commit**: `05dab2d2b5f1f3e40029776e339a486d70491079`
- **License**: MIT License (Copyright (c) 2025 TurboKach)

## Component Adaptations

All components in this directory have been adapted for Project Muteki:
1. **Uncontrolled Demo Logic Removed**: Mock timers, fixed sequence loops, and static dummy data have been replaced with controlled React props driven by Muteki's real backend SSE streams, tool execution ledger, and command dispatch system.
2. **Design Token & Palette Engine Integration**: Styles are wired to Muteki's high-contrast light and dark palette engine (`--bg`, `--panel`, `--text`, `--muted`, `--blue`, `--green`, `--amber`, `--red`, `--line`, etc.), ensuring full consistency across `/task`, `/run/[id]`, and `/chat`.
3. **Accessibility & Keyboard Navigation**: Retained accessible ARIA states, keyboard shortcuts, and semantic markup.

## Included Components

- `t-shimmer-text` CSS class: Linear gradient text sweep (transitions.dev).
- `atoms/StreamText.tsx`: Blur-in streaming text word animator with trailing caret.
- `thinking.tsx`: Expandable reasoning and thought process timeline with duration and status.
- `streaming-text.tsx`: Markdown/streaming text renderer with actions (copy, retry, fork) and citations.
- `loading-state.tsx`: Pixel-matrix loader with live elapsed counter for long-running agent turns.
- `approval-card.tsx`: Human-in-the-loop permission and action approval card.
- `tool-chips.tsx`: Compact sequential tool-call chips with status, duration, args, and drawer triggers.
- `task-rows.tsx`: Progressively revealed task and step status rows.
- `diff-table.tsx`: File diff presentation table with addition/deletion counters and chunk inspection.
- `code-block.tsx`: Code block with live line streaming, syntax accents, line numbers, and copy action.
- `context-cards.tsx`: Retrieved knowledge and attachment context cards.
- `sidebar-nav.tsx`: Workspace and conversation thread sidebar navigation with gliding highlight.
- `search.tsx`: Live command and conversation filter search bar.
- `prompt-bar.tsx`: Floating/sticky composer with model selector, reasoning effort, permission mode, attachments, and controls.
- `chat.tsx`: Multi-stage chat thread view and message sections.
