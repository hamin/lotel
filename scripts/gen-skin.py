#!/usr/bin/env python3
"""Bake the use-by design into the binary: crates/lotel/src/skin.rs.

One-way door: design/useby owns the look; the app embeds its output. After
any change to design/useby/build.py or useby.css run:

    python3 design/useby/build.py --check && python3 scripts/gen-skin.py

Assembly (order matters): the self-hosted @font-face blocks first (the woff2
files ride inside the binary, served from /assets/fonts/ by src/assets.rs —
no Google Fonts at runtime), then the generated token blocks (palettes x
colourways, type pairings, styles), then the component sheet. The panel's
option swatches style off [label|a][data-p]; build.py's tokens_css already
emits the anchor twins the port's link-based panel needs, so nothing is
duplicated here. All values are precomputed hex for the owner's Chrome 98.
"""
import ast
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
USEBY = ROOT / "design" / "useby"
BUILD = USEBY / "build.py"
OUT = ROOT / "crates" / "lotel" / "src" / "skin.rs"

IMPORT_RE = re.compile(r"@import\s+url\((.*?)\);?\s*")

# The shipped faces: (family, font-style, font-weight, file). One entry per
# @font-face, mirroring what the Google Fonts CSS API serves for FONT_LINK
# (design-time only, via <link> in the mocks) with the URLs rewritten to the
# embedded copies under crates/lotel/assets/fonts/, which src/assets.rs
# serves at /assets/fonts/<file>. Variable families ship one file across
# their whole requested weight range (Static instances would break
# Recursive's type system: its tokens drive --fv-* axes); DM Mono is static,
# so it carries one file per weight. All SIL OFL: texts sit in the fonts'
# LICENSES/ directory.
FACES = [
    ("Recursive", "oblique 0deg 15deg", "300 1000", "recursive-latin.woff2"),
    ("Figtree", "normal", "400 800", "figtree-latin.woff2"),
    ("Spline Sans Mono", "normal", "400 600", "spline-sans-mono-latin.woff2"),
    ("Onest", "normal", "400 700", "onest-latin.woff2"),
    ("DM Mono", "normal", "400", "dm-mono-400-latin.woff2"),
    ("DM Mono", "normal", "500", "dm-mono-500-latin.woff2"),
]

# Google's latin subset range, identical for every face: glyphs outside it
# never trigger a download, so the latin files stay the only ones shipped.
LATIN_RANGE = (
    "U+0000-00FF, U+0131, U+0152-0153, U+02BB-02BC, U+02C6, U+02DA, U+02DC, "
    "U+0304, U+0308, U+0329, U+2000-206F, U+20AC, U+2122, U+2191, U+2193, "
    "U+2212, U+2215, U+FEFF, U+FFFD"
)


def font_faces() -> str:
    """The @font-face rules, byte-identical in meaning to the Google CSS API's
    latin blocks — swap so text paints in the system face until the bytes
    arrive from the same binary."""
    return "".join(
        "@font-face {{ font-family: '{}'; font-style: {}; font-weight: {}; "
        "font-display: swap; src: url('/assets/fonts/{}') format('woff2'); "
        "unicode-range: {}; }}\n".format(fam, style, weight, file, LATIN_RANGE)
        for fam, style, weight, file in FACES
    )


def value_of(src: str, name: str):
    tree = ast.parse(src)
    for node in tree.body:
        if isinstance(node, ast.Assign) and getattr(node.targets[0], "id", None) == name:
            # build.py spells some token maps with dict(...); fold those back
            # into literals so strict literal_eval can read them
            return ast.literal_eval(ast.fix_missing_locations(_undict(node.value)))
    raise SystemExit(f"{name} not found in design/useby/build.py")


class _DictCalls(ast.NodeTransformer):
    """Rewrite dict(k=v, ...) calls into plain dict literals."""

    def visit_Call(self, node: ast.Call) -> ast.AST:
        self.generic_visit(node)
        if isinstance(node.func, ast.Name) and node.func.id == "dict" and not node.args:
            return ast.Dict(keys=[ast.Constant(k.arg) for k in node.keywords],
                            values=[v.value for v in node.keywords])
        return node


def _undict(node: ast.AST) -> ast.AST:
    return _DictCalls().visit(node)


def pairs_of(table: dict) -> str:
    return "\n".join('    ("{}", "{}"),'.format(k, v["label"]) for k, v in table.items())


def main() -> None:
    tokens = (USEBY / "tokens.css").read_text()
    sheet = (USEBY / "useby.css").read_text()

    # the lists ride with the design: parsed from build.py itself, so the
    # panel's option tables can never drift from the token blocks
    src = BUILD.read_text()
    pals = value_of(src, "P")
    fonts = value_of(src, "FONTS")
    styles = value_of(src, "STYLES")
    pals_rows = pairs_of(pals)
    fonts_rows = pairs_of(fonts)
    styles_rows = pairs_of(styles)

    # the faces ride at the top; any stray @import in the source sheets is a
    # remote dependency and gets stripped
    css = font_faces()
    for body in (tokens, sheet):
        css += IMPORT_RE.sub("", body) + "\n"

    # comments are dead weight in a shipped binary — and design notes about
    # the very syntax we police should not trip the guard as mere text
    css = re.sub(r"/\*.*?\*/", "", css, flags=re.S)

    # the binary's Chrome-98 promise: no modern-only color syntax and no
    # :has() ships at all (the use-by sheet is precomputed hex by contract),
    # and no remote CSS at all — the fonts are embedded, so any @import or
    # googleapis reference would be a runtime network dependency
    for banned in ("oklch", "color-mix", ":has(", "@import", "googleapis"):
        if banned in css:
            raise SystemExit(f"{banned} leaked into the shipped CSS")
    if '"##"' in css:
        raise SystemExit('raw-string terminator "##" in the CSS')

    OUT.write_text(f"""//! GENERATED by scripts/gen-skin.py — do not hand-edit.
//! Source of truth: design/useby (build.py + tokens.css + useby.css).
//! Restyle flow: edit design/useby/build.py or useby.css, run
//! `python3 design/useby/build.py --check`, then `python3 scripts/gen-skin.py`,
//! rebuild.

/// The whole skin: the four palettes' token blocks x colourways, the three
/// type pairings and the two structural styles (attribute-mapped, all
/// precomputed hex for the owner's Chrome 98) + the component sheet.
pub(crate) const CSS: &str = r##"
{css}
"##;

/// (value, label) per palette; value is the data-pal attribute and the
/// token-block key, label is what the settings panel shows.
pub(crate) const PALETTES: &[(&str, &str)] = &[
{pals_rows}
];

/// (value, label) per type pairing; value is the data-font attribute.
pub(crate) const FONTS: &[(&str, &str)] = &[
{fonts_rows}
];

/// The colourway choices: the data-scheme value and its panel label.
/// "system" follows the OS light/dark preference (shown as "auto").
pub(crate) const SCHEMES: &[(&str, &str)] =
    &[("system", "auto"), ("day", "day"), ("night", "night")];

/// (value, label) per structural style; value is the data-style attribute.
/// The settings panel's fourth axis.
pub(crate) const STYLES: &[(&str, &str)] = &[
{styles_rows}
];
""")
    print(f"skin.rs: {len(css)} bytes of CSS, {len(pals)} palettes, "
          f"{len(fonts)} fonts, {len(styles)} styles")


if __name__ == "__main__":
    main()
