"""Build a tiny TTF exercising the GSUB engine: a Type-4 ligature (f i -> fi)
and a Type-6 chained-context single sub (equal preceded by less -> eq_le).

Produces src/gui/testdata/ligtest.ttf, loaded via include_bytes! in the gui
shaper/renderer tests. Run with a Python that has fonttools installed:

    python extra/gen_ligtest_font.py
"""
import io

from fontTools.fontBuilder import FontBuilder
from fontTools.feaLib.builder import addOpenTypeFeaturesFromString
from fontTools.pens.ttGlyphPen import TTGlyphPen

GLYPHS = [".notdef", "space", "equal", "greater", "less", "f", "i", "fi", "eq_le"]
UPEM = 1000


def box():
    pen = TTGlyphPen(None)
    pen.moveTo((100, 0))
    pen.lineTo((100, 700))
    pen.lineTo((500, 700))
    pen.lineTo((500, 0))
    pen.closePath()
    return pen.glyph()


fb = FontBuilder(UPEM, isTTF=True)
fb.setupGlyphOrder(GLYPHS)
fb.setupCharacterMap(
    {0x20: "space", 0x3C: "less", 0x3D: "equal", 0x3E: "greater", 0x66: "f", 0x69: "i"}
)
fb.setupGlyf({g: (TTGlyphPen(None).glyph() if g in (".notdef", "space") else box()) for g in GLYPHS})
fb.setupHorizontalMetrics({g: (600, 0) for g in GLYPHS})
fb.setupHorizontalHeader(ascent=800, descent=-200)
fb.setupNameTable({"familyName": "LigTest", "styleName": "Regular"})
fb.setupOS2(sTypoAscender=800, sTypoDescender=-200)
fb.setupPost()
addOpenTypeFeaturesFromString(
    fb.font,
    """
feature liga {
    sub f i by fi;
} liga;
feature calt {
    sub less equal' by eq_le;
} calt;
""",
)

buf = io.BytesIO()
fb.font.save(buf)
data = buf.getvalue()
with open("src/gui/testdata/ligtest.ttf", "wb") as f:
    f.write(data)
print("wrote src/gui/testdata/ligtest.ttf", len(data), "bytes")
