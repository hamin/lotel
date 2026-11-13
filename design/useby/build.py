#!/usr/bin/env python3
"""Use-by: build the lotel restyle mocks.

  python3 design/useby/build.py            # needs lotel serving on 127.0.0.1:8080
  python3 design/useby/build.py --check    # contrast report only

Writes:
  design/useby/tokens.css   palettes x colourways, type pairings, styles (generated)
  design/useby/pages/*.html the app's real markup, fetched live, old skin removed,
                            new sheets linked, a few marked markup deltas applied

Hand-written beside it: useby.css (the component sheet), styleguide.html.
Everything is precomputed hex (no oklch, no color-mix, no :has): the owner's
browser is Chrome 98, same constraint as crates/lotel/src/skin.rs.
"""
import json, re, sys, html, urllib.request, urllib.parse, datetime
from pathlib import Path

HERE = Path(__file__).resolve().parent
BASE = "http://127.0.0.1:8080"

# --------------------------------------------------------------------------
# Palettes. Token names match what skin.rs and the inline chart JS already
# read (--color-accent, --color-accent-2, --color-bad, --color-muted, ...),
# plus the new roles: warn, info, forget (the use-by sticker), svc0..5.
# accent   = what you do (primary control, current page, selection)
# accent-2 = what the probe measured (series, facet bars)
# bad      = the fault
# forget   = when it forgets (retention stickers, the horizon)
# --------------------------------------------------------------------------
P = {
 "lime": {
  "label": "lime",
  "day": dict(paper="#F5F6FA", paper2="#ECEEF4", paper3="#E1E5EE", ink="#161A2C", ink2="#343A52",
              muted="#565C75", muted2="#8B91A8", rule="#D3D8E3", rule2="#E6E9F0",
              accent="#C6EF4E", accent_ink="#161A2C", accent_deep="#4A6600",
              accent2="#3156E6", accent2_soft="#DCE4FF",
              bad="#D9363E", bad_soft="#FDE4E4", bad_ink="#A3161E",
              warn="#F2A516", warn_soft="#FFF0CF", warn_ink="#714500",
              info_soft="#E0E7FF", info_ink="#2843B8",
              forget="#FF8CC0", forget_ink="#161A2C", focus="#3156E6", sel="#E4F7AE",
              svc=["#3156E6", "#D6336C", "#0E9F7E", "#7048E8", "#E8590C", "#1C8AA8"]),
  "night": dict(paper="#12141E", paper2="#1A1D2A", paper3="#232738", ink="#ECEEF6", ink2="#C5C9D8",
              muted="#9AA0B6", muted2="#6B7189", rule="#2D3245", rule2="#222636",
              accent="#C6EF4E", accent_ink="#12141E", accent_deep="#C6EF4E",
              accent2="#7D98FF", accent2_soft="#232C52",
              bad="#FF6B70", bad_soft="#3A1D24", bad_ink="#FF9DA0",
              warn="#F5B53D", warn_soft="#3A2E14", warn_ink="#F5C56A",
              info_soft="#1F2847", info_ink="#A6B8FF",
              forget="#FF8CC0", forget_ink="#12141E", focus="#C6EF4E", sel="#3A4A18",
              svc=["#7D98FF", "#F06595", "#38D9A9", "#9775FA", "#FF922B", "#3BC9DB"]),
 },
 "sherbet": {
  "label": "sherbet",
  "day": dict(paper="#F7F5FA", paper2="#EFEBF5", paper3="#E4DEEE", ink="#221A30", ink2="#43395A",
              muted="#625877", muted2="#9890AB", rule="#DBD4E6", rule2="#EAE5F1",
              accent="#FF9B54", accent_ink="#221A30", accent_deep="#A34000",
              accent2="#0B8A84", accent2_soft="#D2F1EE",
              bad="#D6304A", bad_soft="#FCE3E7", bad_ink="#A0122C",
              warn="#F0C419", warn_soft="#FFF4C7", warn_ink="#665000",
              info_soft="#D5F0EE", info_ink="#075F5B",
              forget="#B9A2FF", forget_ink="#221A30", focus="#A34000", sel="#FFE3CF",
              svc=["#0B8A84", "#D6336C", "#7048E8", "#E8590C", "#2F6FDB", "#5C940D"]),
  "night": dict(paper="#17121F", paper2="#201929", paper3="#2A2235", ink="#F1ECF7", ink2="#CFC6DC",
              muted="#A79CB9", muted2="#74698A", rule="#352C44", rule2="#271F33",
              accent="#FF9B54", accent_ink="#17121F", accent_deep="#FFB27A",
              accent2="#3FD0C6", accent2_soft="#133B3A",
              bad="#FF6B81", bad_soft="#3D1B26", bad_ink="#FF9DAC",
              warn="#F5D04A", warn_soft="#3A3216", warn_ink="#F5DA7A",
              info_soft="#123634", info_ink="#7FE3DA",
              forget="#C5B1FF", forget_ink="#17121F", focus="#FFB27A", sel="#4A2E1C",
              svc=["#3FD0C6", "#F06595", "#9775FA", "#FF922B", "#74A7FF", "#94D82D"]),
 },
 "riso": {
  "label": "riso",
  "day": dict(paper="#F6F7F9", paper2="#EDEFF3", paper3="#E2E6ED", ink="#1B1F3B", ink2="#383E62",
              muted="#585E80", muted2="#8E93AE", rule="#D5D9E4", rule2="#E7EAF0",
              accent="#FFD84A", accent_ink="#1B1F3B", accent_deep="#7A5C00",
              accent2="#3D5AFE", accent2_soft="#DFE5FF",
              bad="#E03131", bad_soft="#FDE2E2", bad_ink="#A51111",
              warn="#FF9F1C", warn_soft="#FFEBD1", warn_ink="#7F4500",
              info_soft="#DFE5FF", info_ink="#2A40C9",
              forget="#FF7AB8", forget_ink="#1B1F3B", focus="#3D5AFE", sel="#FFF1B0",
              svc=["#3D5AFE", "#E8368F", "#0B8A64", "#7950F2", "#D9480F", "#1098AD"]),
  "night": dict(paper="#13152A", paper2="#1B1E36", paper3="#252944", ink="#EEF0FA", ink2="#C8CCE4",
              muted="#9EA4C4", muted2="#6C7294", rule="#2E3354", rule2="#222642",
              accent="#FFD84A", accent_ink="#13152A", accent_deep="#FFD84A",
              accent2="#8197FF", accent2_soft="#232B5C",
              bad="#FF6B6B", bad_soft="#3D1A22", bad_ink="#FF9F9F",
              warn="#FFB454", warn_soft="#3B2A16", warn_ink="#FFC680",
              info_soft="#1E2550", info_ink="#A9B8FF",
              forget="#FF8CC6", forget_ink="#13152A", focus="#FFD84A", sel="#4A421A",
              svc=["#8197FF", "#F783AC", "#38D9A9", "#9775FA", "#FFA94D", "#3BC9DB"]),
 },
 "mint": {
  "label": "mint",
  "day": dict(paper="#F4F7F7", paper2="#EAF0F0", paper3="#DDE6E6", ink="#0F2328", ink2="#2B4248",
              muted="#4E6368", muted2="#869A9E", rule="#CFDBDC", rule2="#E3EBEB",
              accent="#5CE0B8", accent_ink="#0F2328", accent_deep="#086B50",
              accent2="#6741D9", accent2_soft="#E8E0FF",
              bad="#E03131", bad_soft="#FDE2E2", bad_ink="#A51111",
              warn="#F59F00", warn_soft="#FFF0C2", warn_ink="#704800",
              info_soft="#E8E0FF", info_ink="#5230B8",
              forget="#FFA94D", forget_ink="#0F2328", focus="#086B50", sel="#CFF6EA",
              svc=["#6741D9", "#D6336C", "#1C7ED6", "#E8590C", "#087F5B", "#AE3EC9"]),
  "night": dict(paper="#0E1A1D", paper2="#142327", paper3="#1C2E33", ink="#E8F2F2", ink2="#C1D2D3",
              muted="#93AAAD", muted2="#627A7E", rule="#26393E", rule2="#1B2B2F",
              accent="#5CE0B8", accent_ink="#0E1A1D", accent_deep="#5CE0B8",
              accent2="#A68BFF", accent2_soft="#2A2350",
              bad="#FF6B6B", bad_soft="#3A1A1E", bad_ink="#FF9F9F",
              warn="#FCC419", warn_soft="#3A3014", warn_ink="#FFD866",
              info_soft="#2A2350", info_ink="#C2B0FF",
              forget="#FFB86B", forget_ink="#0E1A1D", focus="#5CE0B8", sel="#16463A",
              svc=["#A68BFF", "#F06595", "#4DABF7", "#FF922B", "#38D9A9", "#DA77F2"]),
 },
}
DEFAULT = dict(pal="lime", scheme="day", font="recursive", style="chunky")

FONTS = {
 # one variable family, three voices: casual heads, linear UI, mono data
 "recursive": dict(label="Recursive (one family, three voices)",
                   display='"Recursive", ui-sans-serif, system-ui, sans-serif',
                   body='"Recursive", ui-sans-serif, system-ui, sans-serif',
                   mono='"Recursive", ui-monospace, monospace',
                   fv_display='"CASL" 1, "MONO" 0', fv_body='"CASL" 0, "MONO" 0',
                   fv_mono='"CASL" 0, "MONO" 1', fv_figure='"CASL" 0.6, "MONO" 1',
                   display_weight="760", tracking="-0.01em"),
 "figtree": dict(label="Figtree + Spline Sans Mono",
                 display='"Figtree", ui-sans-serif, system-ui, sans-serif',
                 body='"Figtree", ui-sans-serif, system-ui, sans-serif',
                 mono='"Spline Sans Mono", ui-monospace, monospace',
                 fv_display="normal", fv_body="normal", fv_mono="normal", fv_figure="normal",
                 display_weight="760", tracking="-0.02em"),
 "onest": dict(label="Onest + DM Mono",
               display='"Onest", ui-sans-serif, system-ui, sans-serif',
               body='"Onest", ui-sans-serif, system-ui, sans-serif',
               mono='"DM Mono", ui-monospace, monospace',
               fv_display="normal", fv_body="normal", fv_mono="normal", fv_figure="normal",
               display_weight="700", tracking="-0.02em"),
}
FONT_LINK = ("https://fonts.googleapis.com/css2?"
             "family=Recursive:slnt,wght,CASL,CRSV,MONO@-15..0,300..1000,0..1,0..1,0..1"
             "&family=Figtree:wght@400;700;800&family=Spline+Sans+Mono:wght@400;600"
             "&family=Onest:wght@400;700&family=DM+Mono:wght@400;500&display=swap")

STYLES = {
 # playful: ink outlines, hard offset shadow on what you press, tilted stickers
 "chunky": dict(label="chunky", line_w="1.5px", line_c="var(--color-ink)", pop="3px 3px 0 var(--color-edge)",
                pop_s="2px 2px 0 var(--color-edge)", r_ctl="10px", r_card="14px", r_chip="8px", tilt="-1.5deg",
                card_line="1.5px solid var(--color-ink)"),
 # analytical: hairlines, no shadow, tighter radii
 "crisp": dict(label="crisp", line_w="1px", line_c="var(--color-rule)", pop="none", pop_s="none",
               r_ctl="6px", r_card="8px", r_chip="4px", tilt="0deg",
               card_line="1px solid var(--color-rule)"),
}

# --------------------------------------------------------------------------
def lum(h):
    h = h.lstrip("#"); r, g, b = (int(h[i:i+2], 16) / 255 for i in (0, 2, 4))
    f = lambda c: c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4
    return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)

def cr(a, b):
    la, lb = sorted((lum(a), lum(b)), reverse=True); return (la + 0.05) / (lb + 0.05)

PAIRS = [("ink", "paper", 7), ("ink", "paper2", 7), ("muted", "paper", 4.5), ("muted", "paper2", 4.5),
         ("muted", "paper3", 4.5), ("accent_ink", "accent", 4.5), ("accent_deep", "paper", 4.5),
         ("accent2", "paper", 3), ("bad", "paper", 3), ("bad_ink", "bad_soft", 4.5),
         ("warn_ink", "warn_soft", 4.5), ("info_ink", "info_soft", 4.5), ("forget_ink", "forget", 4.5),
         ("focus", "paper", 3), ("ink", "sel", 7), ("muted2", "paper", 2.2)]

def check():
    bad = 0
    for name, p in P.items():
        for scheme in ("day", "night"):
            t = p[scheme]
            for a, b, need in PAIRS:
                v = cr(t[a], t[b])
                if v < need:
                    bad += 1; print(f"FAIL {name}/{scheme}: {a} on {b} = {v:.2f} (< {need})")
            for i, s in enumerate(t["svc"]):
                if cr(s, t["paper"]) < 3:
                    bad += 1; print(f"FAIL {name}/{scheme}: svc{i} {s} on paper = {cr(s, t['paper']):.2f}")
    print(f"contrast: {bad} failing pairs")
    return bad

def block(t, scheme):
    out = [f"color-scheme: {'dark' if scheme == 'night' else 'light'};"]
    m = dict(paper="paper", paper2="paper-2", paper3="paper-3", ink="ink", ink2="ink-2", muted="muted",
             muted2="muted-2", rule="rule", rule2="rule-2", accent="accent", accent_ink="accent-ink",
             accent_deep="accent-deep", accent2="accent-2", accent2_soft="accent-2-soft", bad="bad",
             bad_soft="bad-soft", bad_ink="bad-ink", warn="warn", warn_soft="warn-soft", warn_ink="warn-ink",
             info_soft="info-soft", info_ink="info-ink", forget="forget", forget_ink="forget-ink",
             focus="focus", sel="sel")
    for k, v in m.items():
        out.append(f"--color-{v}: {t[k]};")
    for i, s in enumerate(t["svc"]):
        out.append(f"--svc{i}: {s};")
    # compatibility names the existing Rust templates and chart JS read
    out.append(f"--color-edge: {t['ink'] if scheme == 'day' else '#05060B'};")
    out += ["--color-on-series: var(--color-paper);", "--color-accent-hover: var(--color-accent);",
            "--color-usel: var(--color-paper-2);", "--color-badge-bad-line: var(--color-bad);",
            "--color-badge-warn: var(--color-warn-ink);", "--color-badge-warn-line: var(--color-warn);",
            "--color-badge-info: var(--color-info-ink);", "--color-badge-info-line: var(--color-accent-2);"]
    return " ".join(out)

def tokens_css():
    L = ["/* GENERATED by design/useby/build.py: palettes x colourways, type pairings, styles.",
         " * Attribute contract (same as the shipped skin): html[data-pal] [data-scheme] [data-font] + new [data-style].",
         " * All hex, no oklch / color-mix: the owner runs Chrome 98. */",
         ":root {",
         "  --space-3xs: 2px; --space-2xs: 4px; --space-xs: 8px; --space-sm: 12px; --space-md: 16px; --space-lg: 24px; --space-xl: 40px; --space-2xl: 64px; --space-3xl: 96px;",
         "  --text-2xs: 0.6875rem; --text-xs: 0.75rem; --text-sm: 0.8125rem; --text-md: 0.9375rem; --text-lg: 1.125rem; --text-xl: 1.375rem; --text-2xl: 1.75rem; --text-3xl: 2.25rem;",
         "  --ease-out: cubic-bezier(0.16, 1, 0.3, 1); --ease-in: cubic-bezier(0.7, 0, 0.84, 0); --ease-in-out: cubic-bezier(0.65, 0, 0.35, 1);",
         "  --dur-micro: 120ms; --dur-short: 220ms; --dur-long: 420ms;",
         "  --nav-w: 216px; --top-h: 60px; --container: 1440px; --gutter: 40px;",
         "  --z-base: 1; --z-raised: 10; --z-dropdown: 100; --z-sticky: 200; --z-sticky-nav: 300; --z-modal: 400; --z-toast: 500; --z-tooltip: 600;",
         "}"]
    for name, p in P.items():
        L.append(f"html[data-pal=\"{name}\"], html[data-pal=\"{name}\"][data-scheme=\"day\"] {{ {block(p['day'], 'day')} }}")
        L.append(f"html[data-pal=\"{name}\"][data-scheme=\"night\"] {{ {block(p['night'], 'night')} }}")
        L.append(f"@media (prefers-color-scheme: dark) {{ html[data-pal=\"{name}\"][data-scheme=\"system\"] {{ {block(p['night'], 'night')} }} }}")
    for name, f in FONTS.items():
        L.append(f"html[data-font=\"{name}\"] {{ --font-display: {f['display']}; --font-body: {f['body']}; --font-mono: {f['mono']}; "
                 f"--fv-display: {f['fv_display']}; --fv-body: {f['fv_body']}; --fv-mono: {f['fv_mono']}; --fv-figure: {f['fv_figure']}; "
                 f"--display-weight: {f['display_weight']}; --tracking-display: {f['tracking']}; }}")
    for name, s in STYLES.items():
        L.append(f"html[data-style=\"{name}\"] {{ --line-w: {s['line_w']}; --line-c: {s['line_c']}; --pop: {s['pop']}; --pop-s: {s['pop_s']}; "
                 f"--r-ctl: {s['r_ctl']}; --r-card: {s['r_card']}; --r-chip: {s['r_chip']}; --tilt: {s['tilt']}; --card-line: {s['card_line']}; }}")
    for name, p in P.items():
        d = p["day"]
        for cls, key in (("a", "accent"), ("b", "accent2"), ("c", "bad"), ("d", "forget")):
            L.append(f'.pop a[data-p="{name}"] .sw.{cls} {{ background: {d[key]}; }}')
    return "\n".join(L) + "\n"

# --------------------------------------------------------------------------
# Page build: the live markup, restyled.
# --------------------------------------------------------------------------
def get(path):
    with urllib.request.urlopen(BASE + path, timeout=30) as r:
        return r.read().decode("utf-8")

HEAD_BOOT = """<script>
/* design-time only: apply the look from ?pal=&scheme=&font=&style= or localStorage before paint */
(function(){var d=document.documentElement,q=new URLSearchParams(location.search),K=['pal','scheme','font','style'],
D=%s;K.forEach(function(k){var v=q.get(k);try{if(v)localStorage.setItem('useby-'+k,v);v=v||localStorage.getItem('useby-'+k);}catch(e){}
d.setAttribute('data-'+k,v||D[k]);});})();
</script>""" % json.dumps(DEFAULT)

FOOT_SYNC = """<script>
/* design-time only: mark the current choices in the settings popover */
(function(){var d=document.documentElement;document.querySelectorAll('.pop a.box[data-k]').forEach(function(a){
a.classList.toggle('on',d.getAttribute('data-'+a.getAttribute('data-k'))===a.getAttribute('data-v'));});
var w=document.querySelector('.set .who');if(w)w.textContent=['pal','scheme','font','style'].map(function(k){return d.getAttribute('data-'+k);}).join(' / ');})();
</script>"""

def settings_popover():
    def grp(key, label, items):
        boxes = []
        for v, lab in items:
            sw = ""
            if key == "pal":
                sw = '<i class="sw a" aria-hidden="true"></i><i class="sw b" aria-hidden="true"></i><i class="sw c" aria-hidden="true"></i><i class="sw d" aria-hidden="true"></i>'
            boxes.append(f'<a class="box" data-k="{key}" data-v="{v}" data-p="{v if key == "pal" else ""}" href="?{key}={v}">{sw}{html.escape(lab)}</a>')
        return f'<div class="grp" role="group" aria-label="{label}"><span class="gk">{label}</span>{"".join(boxes)}</div>'
    return ('<details class="set"><summary><span class="pv" aria-hidden="true"><i class="p"></i><i class="a"></i><i class="c"></i></span>'
            '<span class="who">look</span></summary><div class="pop">'
            + grp("pal", "Palette", [(k, v["label"]) for k, v in P.items()])
            + grp("scheme", "Colourway", [("system", "auto"), ("day", "day"), ("night", "night")])
            + grp("font", "Type", [(k, v["label"]) for k, v in FONTS.items()])
            + grp("style", "Style", [(k, v["label"]) for k, v in STYLES.items()])
            + '<p class="law">The accent is what you do. The second hue is what the probe measured. Red is the fault. The sticker is when it forgets.</p>'
            '</div></details>')

def ttl_strip(ttls):
    chips = "".join(f'<span class="useby"><span class="useby-k">{k}</span> <b>{v}</b></span>' for k, v in ttls)
    return f'<div class="useby-strip" role="group" aria-label="Retention"><span class="useby-cap">forgets after</span>{chips}</div>'

ROUTES = [(r"^/traces\?trace=[^\"]*", "trace.html"), (r"^/traces[^\"]*", "traces.html"),
          (r"^/metrics\?[^\"]*chart=1[^\"]*", "metric.html"), (r"^/metrics[^\"]*", "metrics.html"),
          (r"^/logs[^\"]*", "logs.html"), (r"^/query[^\"]*", "query.html"), (r"^/services[^\"]*", "services.html"),
          (r"^/storage[^\"]*", "storage.html"), (r"^/$", "home.html")]

def rewrite_links(h):
    def fix(m):
        attr, url = m.group(1), html.unescape(m.group(2))
        for pat, dest in ROUTES:
            if re.match(pat, url):
                return f'{attr}="{dest}"'
        if url.startswith("/"):
            return f'{attr}="#"'
        return m.group(0)
    return re.sub(r'(href|action)="([^"]*)"', fix, h)

def fmt_int(s):
    try:
        return f"{int(s):,}"
    except ValueError:
        return s

def build_page(name, path, ttls, extra=None):
    h = get(path)
    # drop the shipped skin (the first <style> in <head>) and the old settings popover
    h = re.sub(r"<head>(.*?)<style>.*?</style>", r"<head>\1", h, count=1, flags=re.S)
    h = re.sub(r'<details class="set">.*?</details>', "@@SET@@", h, count=1, flags=re.S)
    h = h.replace(' data-pal="honeycomb" data-scheme="system" data-font="term"', "")
    # live polling cannot reach a server from file://: keep the markup, stub the poller
    h = re.sub(r"<script>\s*window\.LotelLive=.*?</script>", "<script>window.LotelLive={sub:function(){}};</script>", h, flags=re.S)
    if extra:
        h = extra(h)
    h = rewrite_links(h)
    # markup delta 1: retention stickers in the top bar (values from capped_status ttl)
    h = h.replace("@@SET@@", ttl_strip(ttls) + settings_popover())
    # markup delta 2: thousands separators on stat tiles
    h = re.sub(r'<div class="n">(\d+)</div>', lambda m: f'<div class="n">{fmt_int(m.group(1))}</div>', h)
    head = (HEAD_BOOT + f'<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="stylesheet" href="{FONT_LINK}">'
            '<link rel="stylesheet" href="../tokens.css"><link rel="stylesheet" href="../useby.css">')
    h = h.replace("</title>", "</title>" + head, 1)
    h = h.replace("</body>", FOOT_SYNC + "</body>", 1)
    (HERE / "pages" / f"{name}.html").write_text(h)
    print(f"  pages/{name}.html  {len(h)//1024} KB")

def inline_live(topic, query, target_id):
    frag = get(f"/api/live?topic={topic}&{query}")
    def f(h):
        return re.sub(rf'(<div id="{target_id}"[^>]*>).*?(</div>\s*(?:<h2>|<script>|</main>))',
                      lambda m: m.group(1) + frag + m.group(2), h, count=1, flags=re.S)
    return f

def inline_series(h):
    m = re.search(r"var P = (\{.*?\});", h)
    if not m:
        return h
    p = json.loads(m.group(1))
    qs = urllib.parse.urlencode({"name": p["name"], "service": p["service"], "shape": p["shape"]})
    data = get(f"/api/metrics/series?{qs}")
    h = re.sub(r"fetch\('/api/metrics/series\?name='.*?\.then\(function \(r\) \{ return r\.json\(\); \}\)",
               "Promise.resolve(window.__SERIES)", h, count=1, flags=re.S)
    return h.replace("<script>", f"<script>window.__SERIES={data};</script><script>", 1)

def inline_qchart(h):
    data = get("/api/query/chart")
    h = h.replace("fetch('/api/query/chart' + window.location.search)",
                  "Promise.resolve({json:function(){return window.__QCHART;}})", 1)
    return h.replace("<script>", f"<script>window.__QCHART={data};</script><script>", 1)

def storage_ribbon(h):
    """markup delta 3 (the signature): each table's segments on its retention window."""
    rows = re.findall(r'<tr><td class="mono">([^<]+)</td><td>(\d+)</td><td>(true|false)</td><td>(\d+)</td><td>([^<]+)</td>'
                      r'<td class="muted">(\d*)</td><td class="muted">(\d*)</td><td>([^<]*)</td>', h)
    if not rows:
        return h
    unit = {"d": 86400e6, "h": 3600e6, "m": 60e6}
    now = max(int(r[6]) for r in rows if r[6])
    tables = {}
    for t, sid, sealed, nrows, size, mn, mx, ttl in rows:
        tables.setdefault(t, {"ttl": ttl, "segs": []})["segs"].append((int(sid), sealed == "true", int(nrows), size, int(mn or now), int(mx or now)))
    out = ['<section class="ribbon" aria-labelledby="ribbon-h"><h2 id="ribbon-h">What forgets next</h2>',
           '<p class="muted ribbon-cap">Each table\'s segments laid on its retention window. The left edge is the use-by line: '
           'a segment that slides past it is deleted whole. Now is the newest row in the store.</p><div class="ribbon-rows">']
    for t, d in tables.items():
        m = re.match(r"(\d+)([dhm])", d["ttl"])
        if not m:
            continue
        ttl_us = int(m.group(1)) * unit[m.group(2)]
        start = now - ttl_us
        sealed = [s for s in d["segs"] if s[1]]
        nxt = min(sealed, key=lambda s: s[5]) if sealed else None
        left = ""
        if nxt:
            rem = nxt[5] + ttl_us - now
            dd, hh = int(rem // 86400e6), int((rem % 86400e6) // 3600e6)
            left = f'segment {nxt[0]} forgets in <b>{dd}d {hh}h</b>' if dd else f'segment {nxt[0]} forgets in <b>{hh}h</b>'
        blocks = []
        for sid, is_sealed, nrows, size, mn, mx in d["segs"]:
            l = max(0.0, (mn - start) / ttl_us * 100)
            w = max(0.8, (mx - mn) / ttl_us * 100)
            w = min(w, 100 - l)
            cls = "seg" + ("" if is_sealed else " seg-open")
            tip = f"segment {sid} · {nrows:,} rows · {size} · {'sealed' if is_sealed else 'open'}"
            blocks.append(f'<span class="{cls}" style="left:{l:.2f}%;width:{w:.2f}%" title="{html.escape(tip)}"></span>')
        out.append(f'<div class="ribbon-row"><div class="ribbon-name"><span class="mono">{t}</span>'
                   f'<span class="useby"><b>{d["ttl"]}</b></span></div>'
                   f'<div class="ribbon-track"><span class="ribbon-line" aria-hidden="true"></span>{"".join(blocks)}</div>'
                   f'<div class="ribbon-next muted">{left}</div></div>')
    out.append('<div class="ribbon-axis muted" aria-hidden="true"><span>use-by line</span><span>now</span></div></div></section>')
    return h.replace("<h2>Tables</h2>", "".join(out) + "<h2>Tables</h2>", 1)

def main():
    if "--check" in sys.argv:
        sys.exit(1 if check() else 0)
    check()
    (HERE / "tokens.css").write_text(tokens_css())
    print("  tokens.css")
    (HERE / "pages").mkdir(exist_ok=True)
    storage = get("/storage")
    ttls = []
    for label, table in (("logs", "otlp_logs"), ("traces", "otlp_traces"), ("metrics", "otlp_metrics_gauge")):
        m = re.search(rf'<td class="mono">{table}</td>(?:<td>[^<]*</td>|<td class="muted">[^<]*</td>){{6}}<td>([^<]*)</td>', storage)
        ttls.append((label, m.group(1) if m else "?"))
    trace = re.search(r"trace=([0-9a-f]{32})", get("/traces")).group(1)
    metric = html.unescape(re.search(r'href="(/metrics\?shape=gauge[^"]*chart=1)"', get("/metrics")).group(1))
    build_page("home", "/", ttls)
    build_page("logs", "/logs", ttls)
    build_page("traces", "/traces", ttls)
    build_page("trace", f"/traces?trace={trace}", ttls)
    build_page("services", "/services?range=24h", ttls, inline_live("map", "range=24h", "sm-live"))
    build_page("metrics", "/metrics", ttls)
    build_page("metric", metric, ttls, inline_series)
    build_page("storage", "/storage", ttls, storage_ribbon)
    build_page("query", "/query", ttls, inline_qchart)
    src = (HERE / "styleguide.src.html").read_text()
    head = (HEAD_BOOT + f'<link rel="preconnect" href="https://fonts.googleapis.com"><link rel="stylesheet" href="{FONT_LINK}">'
            '<link rel="stylesheet" href="tokens.css"><link rel="stylesheet" href="useby.css">')
    sg = src.replace("@@HEAD@@", head).replace("@@SET@@", ttl_strip(ttls) + settings_popover()).replace("@@FOOT@@", FOOT_SYNC)
    (HERE / "styleguide.html").write_text(sg)
    print("  styleguide.html")

if __name__ == "__main__":
    main()
