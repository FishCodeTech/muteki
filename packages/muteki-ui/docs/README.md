# @muteki/ui — Muteki Tactile Precision Workbench UI System

`@muteki/ui` consolidates Project Muteki's Base UI primitives, Agent Domain components, Icon registry, and third-party engine facades into a single framework-agnostic package with zero application store (`@/` / `apps/frontend`) dependencies.

## 1. Visual Language ("Tactile Precision Workbench")

- **Palette & OKLCH Integration**: Components bind `--ui-*` and `--cx-*` CSS variables styled harmoniously with `apps/frontend/lib/palette-engine.ts`. Light & Dark surfaces invert cleanly without hardcoded hex overrides inside primitives.
- **Tactile Surfaces & Specular Sheen**: Controls (`Button`, `Input`, `Select`, `Card`, overlays) use `1px` crisp borders, a top-edge inner specular highlight (`--ui-sheen` / `.ui-surface-tactile`), a `6px / 8px / 12px / 14px` radius ladder, `2px` focus rings (`--cx-focus`), and tabular numerals (`cx-tabular`).
- **Reduced Motion**: Every animated primitive and Agent component respects both OS `prefers-reduced-motion` and `<html data-motion="reduce">` via `useReducedMotion()` and CSS media/attribute selectors.

## 2. Headless Core & WebKit / WKWebView Compatibility Rules

Built on `@ui-sdk/core` and `@ui-sdk/react` (`0.1.0-beta.13`):

1. **`[popover]` UA Margin/Padding Reset**: macOS WebKit applies `padding: 0.25em` and `height: fit-content` to native top-layer `[popover]` elements. All Popover/Menu/Select/Tooltip/HoverCard containers reset `padding: 0` via `.muteki-popover` rules in `styles.css`.
2. **Flex Layout Inside `[popover]`**: Inside top-layer containers whose intrinsic height is `fit-content`, never put `flex-1` (`flex-basis: 0%`) on a scroll body — WKWebView can collapse body height to `0px`. Use `flex-auto min-h-0` on scrollable bodies and `shrink-0` on headers/footers.
3. **Enter Animations via CSS `@keyframes`**: Overlay enter transitions use CSS `@keyframes` keyed on `[data-open]` / `[data-state="open"]` rather than JS `requestAnimationFrame` opacity transitions so WebKit background tab / WKWebView frame throttling never stalls a dialog or popover at `opacity: 0`.
4. **IME Guard**: Popover/Select/Listbox/Dialog Escape handling ignores `event.nativeEvent.isComposing || event.keyCode === 229` via `primitives/_ime.ts`.

## 3. Package Map

- **`packages/muteki-ui/src/primitives/*`**: `Button`, `CopyButton`, `Field` (`Input`, `TextField`, `TextArea`, `SearchInput`, `Checkbox`, `RadioGroup`, `Switch`, `Slider`, `NativeSelect`), `ListBox` (`Select`, `OptionList`), `Dialog` (`Dialog`, `ConfirmDialog`, `Sheet`, `ModalOverlay`), `Popover` (`Popover`, `HoverCard`), `Menu`, `Tooltip`, `Tabs` (`Tabs`, `TabBar`, `SegmentedControl`), `Accordion`, `Card`, `Feedback` (`Badge`, `Chip`, `Alert`, `Callout`, `InlineStatus`, `StatusDot`, `Spinner`, `Skeleton`, `ShimmerText`, `DiffStat`, `EmptyState`), `Progress`, `Toast` (inline SVGs — zero full Lucide runtime pull in root layouts), `Layout`, `Kbd`, `Portal`, `motion`.
- **`packages/muteki-ui/src/agent/*`**: `Message`, `MessageBubble`, `ChatStream` (`Chat`, `REFLOW_PANEL_ATTR`), `Thinking`, `LoadingState`, `ToolResult`, `FileDiff`, `DiffTable`, `CodeBlock`, `TodoList`, `ApprovalCard`, `QuestionCard`, `Markdown`, `PromptBar`, `AgentRoster`.
- **`packages/muteki-ui/src/icons/*`**: Curated semantic icon registry (`<Icon name=... />`, `IconName`) backed by `lucide-react`.
- **`packages/muteki-ui/src/engines/*`**: Isolated facades for `shiki`, `react-markdown`/`remark-*`/`unified`, `katex`, `@xterm/*`, `@codemirror/*`, and `@tanstack/react-virtual`.
