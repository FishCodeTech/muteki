# Terminal symbol fallback

TcodeTerminalSymbols.ttf is a symbol subset of Lilex Nerd Font Mono from
Nerd Fonts 3.5.1 (https://github.com/ryanoasis/nerd-fonts/releases/tag/v3.5.1).
The accompanying OFL and MIT licenses cover the font and added Nerd Font symbols.

Regenerate with `python3 subset.py /path/to/LilexNerdFontMono-Regular.ttf`
using fonttools. Only glyphs absent from the existing bundled Lilex Regular are
retained, plus `m`: GPUI 0.3.3 CosmicTextSystem rejects a fallback face without
that validation glyph. The subset has a distinct family name and uses the
patched mono font's cell-sized symbols. All terminal text and styles still use
the four existing Lilex files. Native clients retain their system fallback.
