"""Build the terminal fallback from Nerd Fonts 3.5.1 LilexNerdFontMono-Regular.ttf.

Usage: python3 subset.py /path/to/LilexNerdFontMono-Regular.ttf
Requires fonttools. The primary terminal faces remain assets/fonts/lilex/*.ttf.
"""
import sys
from pathlib import Path

from fontTools import subset
from fontTools.ttLib import TTFont

font = TTFont(sys.argv[1], recalcTimestamp=False)
primary = TTFont(Path(__file__).parent.parent / 'lilex/Lilex-Regular.ttf')
# CosmicTextSystem::load_family rejects faces without 'm'. Keep that one
# validation glyph, plus symbols absent from the existing primary font.
codepoints = set(font.getBestCmap()) - set(primary.getBestCmap())
codepoints.add(ord('m'))
options = subset.Options()
options.recalc_timestamp = False
subsetter = subset.Subsetter(options=options)
subsetter.populate(unicodes=codepoints)
subsetter.subset(font)
for record in font['name'].names:
    if record.nameID in (1, 4, 6, 16):
        value = 'TcodeTerminalSymbols' if record.nameID == 6 else 'Tcode Terminal Symbols'
        record.string = value.encode(record.getEncoding())
font.save(Path(__file__).parent / 'TcodeTerminalSymbols.ttf')
