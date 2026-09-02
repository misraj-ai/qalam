# Qalam — Arabic PDF Text Extraction Without OCR

> Working name: **qalam** (قلم, "pen"). A Rust core with a Python interface that
> extracts *correct, logical-order Unicode* Arabic text from digitally-born PDFs,
> and honestly reports when a PDF is unrecoverable without OCR.

---

## 1. Goal & Scope

**In scope**
- Extract Arabic text from *digitally-born* (non-scanned) PDFs — text is present
  as glyphs in the content stream, not as a raster image.
- Produce **correct logical Unicode**: base letters (not presentation forms),
  correct reading order (bidi-resolved), ligatures decomposed correctly.
- **Recoverability detection**: when a PDF has no usable text layer (missing/garbage
  `ToUnicode`), say so explicitly instead of emitting corrupted text. This turns a
  silent-corruption failure into an actionable signal ("this one needs OCR").
- **Structured per-page output** (tiered — see below): reading order across columns,
  embedded images, and tables, emitted as typed blocks per page.
- **Visual styling captured alongside the text**: fill colour, font name and effective
  size per run. Colour is free to record while interpreting the content stream and
  impossible to recover afterwards, so we capture it from the start even though nothing
  consumes it in Tier A. It is what makes a later HTML/rich export possible.
- Ship as a `pip install`-able Python package backed by a fast Rust core.

**Scope tiers** — ship each fully before starting the next; structure never blocks text.
- **Tier A — Correct linear text.** The core: logical-order Arabic + recoverability flag.
  This is v1; everything else is additive.
- **Tier B — Reading order + images.** Column/reading-order reconstruction and image
  extraction. Both degrade gracefully with a confidence score.
- **Tier C — Tables.** Tagged + ruled-line tables first; borderless tables as a stretch
  goal (heuristic, and near-greenfield for RTL).
- **Tier D (stretch) — Rich export.** Render the block model to HTML, using the styling
  captured in Tier A. Not a milestone commitment; listed so the layers below never throw
  away information it would need.

**A reality to keep in front of us:** structure (reading order, tables) is *only* stored
in the PDF when it is a **tagged / accessible PDF** (`/StructTreeRoot`). Most files are
untagged, so structure must be **reconstructed geometrically** from glyph/graphics
positions. Every structure feature therefore has two paths: read-if-tagged,
reconstruct-if-not. Reconstruction is best-effort, not exact — it always carries a
confidence score.

**Out of scope (at least for v1)**
- OCR of scanned/image PDFs (we *detect* these and hand off; we don't do OCR).
- PDF *creation* or editing.
- Pixel-perfect layout / reflow. We emit ordered typed blocks, not a faithful visual clone.
- Full graphics-state fidelity: patterns, shadings, transparency groups, blend modes.
  We capture *text* colour, not a rendering engine's worth of state.

**Definition of done**
- *Tier A:* on a corpus of real Arabic PDFs, output logical Unicode that round-trips
  visually (re-shape + re-render matches the original) for recoverable pages, and a clear
  `unrecoverable` flag for the rest.
- *Tier B/C:* typed blocks (text / image / table) per page with reading-order indices and
  per-block confidence; correctness measured against tagged-PDF ground truth where available.

---

## 2. Why this is hard — the five failure modes

A PDF content stream does not store text. It stores instructions to *paint glyphs
at positions*. Extraction is the inverse problem, and Arabic stresses every weak point.

1. **Presentation forms instead of base letters.** Arabic letters change shape by
   position (isolated/initial/medial/final). Many PDFs encode the *shaped* glyph via
   Unicode Presentation Forms-A (U+FB50–FDFF) or Forms-B (U+FE70–FEFF), so you get
   `ﻣﺤﻤﺪ` (positional glyphs) instead of `محمد` (base letters).
2. **Reversed visual order.** Glyphs are laid down left-to-right in *visual* order,
   so an RTL word extracts backward. Needs the Unicode Bidirectional Algorithm (UAX #9).
3. **Multi-character ligatures.** A lam-alef glyph `ﻻ` (U+FEFB) maps to two codepoints
   (U+0644 U+0627). If you normalize *before* reordering, those two characters get
   flipped along with everything else → `لا` becomes `ال`. **Observed live** in the test
   file (§10): the phrase `...إرشادي ولا يغني...` came out as `...إرشادي وال يغني...`.
   The fix is order-of-operations: reorder first, expand ligatures afterward.
4. **Missing / wrong `ToUnicode` CMap.** This map is what converts glyph codes back to
   Unicode. Absent or wrong → **unrecoverable by parsing alone**. This is the hard
   boundary where "no OCR" genuinely cannot win. Must be *detected*, not guessed.
5. **`/ActualText` overrides.** A marked-content span can carry an `/ActualText`
   attribute with the true text, invisible in rendering. A correct extractor honors it
   before falling back to glyph decoding.

---

## 3. Architecture — the extraction pipeline

Layered so each stage is independently testable. Data flows top to bottom.

```
                 ┌──────────────────────────────────────────┐
  PDF bytes ───▶ │ L0  Document parser (lopdf)                │
                 │     xref / object streams / page tree      │
                 └──────────────────────────────────────────┘
                                   │ page content stream + /Resources (fonts)
                                   ▼
                 ┌──────────────────────────────────────────┐
                 │ L1  Content-stream interpreter             │
                 │     BT/ET, Tf, Td/Tm/T*, Tj/TJ             │
                 │     + graphics state: q/Q, cm, g/rg/k/scn, │
                 │       Tr render mode  (→ styling)          │
                 │     → (glyph_code, x, y, adv, style)       │
                 └──────────────────────────────────────────┘
                                   │ positioned, styled glyph codes
                                   ▼
                 ┌──────────────────────────────────────────┐
                 │ L2  Code → Unicode resolution              │
                 │     ActualText > ToUnicode > encoding >    │
                 │     embedded-font cmap                     │
                 │     → positioned Unicode runs              │
                 └──────────────────────────────────────────┘
                                   │ logical? no — still visual + shaped
                                   ▼
                 ┌──────────────────────────────────────────┐
                 │ L3  Arabic reconstruction                  │
                 │     (a) group into lines by y-geometry     │
                 │     (b) bidi reorder (unicode-bidi, UAX#9) │
                 │         — WHILE still presentation forms!  │
                 │     (c) THEN NFKC normalize (forms + ligs  │
                 │         → base letters)                    │
                 └──────────────────────────────────────────┘
                                   │
                    ┌──────────────┴───────────────┐
                    ▼                               ▼
      ┌───────────────────────────┐   ┌───────────────────────────┐
      │ L4  Recoverability detector│   │ L5  Python API (PyO3)      │
      │  confidence score per page │   │  extract_text(path) -> str │
      │  → ok | needs_ocr          │   │  + per-page metadata       │
      └───────────────────────────┘   └───────────────────────────┘
```

**Key design decisions**
- **Positions are ground truth for order**, not the byte order in the stream. Every
  glyph carries `(x, y, advance)` from the text matrix; line/run grouping uses geometry.
- **Order of operations is critical: reorder BEFORE normalizing.** Confirmed on a real
  file (§10). Reorder each run to logical order while ligatures are still single
  presentation glyphs, THEN run NFKC. NFKC-first expands `ﻻ` (U+FEFB) into `ل`+`ا`, which
  the later reorder scrambles into `ال`. `reorder → NFKC` is correct; `NFKC → reorder`
  corrupts every ligature.
- **NFKC does the heavy lifting — but only after reordering.** It folds most presentation
  forms (U+FE7x–FEFx) back to base letters and expands ligatures, clearing failure modes
  #1 and #3 once order is already fixed.
- **A ToUnicode value is a STRING, not a char.** One CID can map to several codepoints
  (real example from §10: CID `01AE → U+0651 U+064B`, shadda + fathatan). Model the map
  as `HashMap<u16, SmallString>` / `HashMap<u16, Vec<char>>`, never `HashMap<u16, char>`.
- **Capture styling at L1 or never.** Colour lives in the *graphics state*, not in the
  text: by the time glyphs reach L3 the `rg`/`g`/`k` operator that set them is long gone.
  So every glyph carries a `Style { fill, font, size, render_mode }` from the moment it
  is emitted. Cost is a few bytes per glyph; the alternative is re-parsing the stream.
- **Style is deduplicated into runs, not stored per character.** Consecutive glyphs
  overwhelmingly share a style, so L3 splits each line into `Span`s at style *changes*.
  A page of body text collapses to a handful of spans.
- **Text render mode `Tr` is both a styling and a correctness signal.** Mode 3 (and 7)
  is *invisible* text — exactly what an OCR layer stapled onto a scan looks like. It
  decides which colour paints the glyph (fill for 0/2, stroke for 1), and mode-3-heavy
  pages are a strong hint for the L4 detector that we are reading someone else's OCR
  rather than born-digital text.
- **Resolution is a fallback chain** (L2), in priority order: `/ActualText` →
  `/ToUnicode` → simple-font `/Encoding` (+ `/Differences`) → embedded font's own cmap.
- **The detector (L4) is the differentiating feature.** Signals: fraction of glyphs
  with no Unicode mapping, fraction mapping to `U+0000`/`.notdef`, presence of a
  `ToUnicode` stream at all, ratio of presentation-form vs base codepoints, and
  entropy/plausibility of the resulting string.

### Output model (Tier B/C)

Text is the trivial reduction of a richer per-page model. A `Page` is an ordered list of
typed blocks:

```
Page  { number, size, rotation, tagged: bool, blocks: Vec<Block> }
Block =
  | Text  { lines: Vec<Line>, bbox, reading_index, confidence }
  | Image { bytes, format, bbox, reading_index }
  | Table { rows: Vec<Vec<Cell>>, bbox, reading_index, confidence }

Line  { spans: Vec<Span>, bbox, baseline }
Span  { text: String, bbox, style: Style }          # one uniform run of styling
Style { fill: Color, font: String, size: f64, render_mode: TextRenderMode }
Color = Gray(f) | Rgb(f,f,f) | Cmyk(f,f,f,f) | Unknown   # -> to_rgb8() for export
```

`extract_text()` just walks blocks in `reading_index` order and joins the `Text` ones —
so Tier A is a *view* over this model, not a separate code path. Every reconstructed block
carries a `confidence`; tagged-PDF blocks are high-confidence by construction.

A `Span` is the unit of uniform styling: the text splits wherever colour, font or size
changes. That is exactly the granularity an HTML `<span style="color:...">` needs, and
plain-text extraction simply ignores it — styling never complicates the Tier A path.

### Structure & object layers (Tier B/C)

These attach around the text core; they never sit in the critical path for linear text.

```
        after L0 (parser) ─┐
                           ▼
        ┌────────────────────────────────────────────┐
        │ L1.5  Structure source                      │
        │   tagged?  →  read /StructTreeRoot + MCIDs   │  (correct, cheap)
        │   untagged →  hand off to L6                 │
        └────────────────────────────────────────────┘

        after L3 (correct logical text) ─┐
                                         ▼
        ┌────────────────────────────────────────────┐
        │ L6  Layout analysis  (untagged fallback)    │
        │   XY-cut: x-projection → column gutters      │
        │           (order columns RIGHT→LEFT for RTL) │
        │           y-projection → lines / paragraphs  │
        │   → Text blocks with reading_index           │
        └────────────────────────────────────────────┘

        from L0 resources, independent of text ─┐
                                                ▼
        ┌────────────────────────────────────────────┐
        │ L7  Object extraction                       │
        │   images: /Resources/XObject /Subtype/Image │
        │     DCTDecode→jpg, Flate+samples→png, …      │
        │     position from CTM at the `Do` operator   │
        │   tables: tagged /Table/TR/TD, else ruled    │
        │     grid from vector graphics (re/l/m/S)     │
        └────────────────────────────────────────────┘
```

- **L1.5 is the answer to "is the structure in the PDF?"** — decided per file at runtime.
- **L6 runs *after* L3**, so column/line grouping operates on correct logical text, not
  presentation forms. RTL column ordering is the one substantive change from a Latin
  layout engine (demonstrated on a synthetic 2-column page, §10.3).
- **L7 images are the low-risk, high-value first structure feature** (Tier B): images are
  first-class objects, so extraction is mostly filter-decode. Tables are the hard tail.

---

## 4. Tech stack & crates

**Core: Rust** — chosen because the hard parts (font/CMap/glyph handling, Unicode
processing) have the strongest ecosystem here, and it compiles to a dependency-free
binary/wheel.

| Concern | Crate | Notes |
|---|---|---|
| PDF object parsing | `lopdf` | xref tables + object streams, dicts, streams. MIT. Foundation. |
| Text-layer reference | `pdf-extract` | Content-stream text decoding on top of lopdf; reference/starting point. |
| Bidi reordering | `unicode-bidi` | Servo team's UAX #9 implementation. Production-grade. |
| Normalization | `unicode-normalization` | NFKC for presentation-form → base-form. |
| Arabic shaping (fwd) | `rustybuzz` | Pure-Rust HarfBuzz port; shaping is the *forward* version of our inverse problem. Reference + validation. |
| Image decode (L7) | `image` + passthrough | Re-encode raw/Flate samples to PNG; DCTDecode bytes are already JPEG (passthrough, no re-encode). |
| Python bindings | `pyo3` + `maturin` | Ships a `pip install`-able wheel. |

**Interface: Python** — thin wrapper over the Rust core via PyO3. Users get a simple
`import qalam; qalam.extract_text("file.pdf")`.

**Why not Go:** `pdfcpu` (Apache-2.0, pure Go) is great for structural work but weak
on text extraction; the best Go text extractor, `unipdf`, is commercial (EULA). Go's
bidi/shaping ecosystem is also thinner. Rust wins on exactly our bottlenecks.

---

## 5. Code organization (Cargo workspace)

```
qalam/
├── Cargo.toml                # workspace root
├── PLAN.md                   # this file
├── README.md
├── rust-toolchain.toml       # pin toolchain (>= 1.85)
│
├── crates/
│   ├── qalam-core/           # the actual library, no Python knowledge
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── parser.rs     # L0: wraps lopdf, page tree, resources
│   │       ├── content.rs    # L1: content-stream interpreter, text matrix
│   │       ├── graphics.rs   # L1: graphics-state stack (q/Q, cm) + colour operators
│   │       ├── font.rs       # L2: ToUnicode / encoding / cmap resolution
│   │       ├── resolve.rs    # L2: ActualText + fallback chain
│   │       ├── arabic.rs     # L3: NFKC normalize + line/run grouping
│   │       ├── bidi.rs       # L3: unicode-bidi wrapper
│   │       ├── detect.rs     # L4: recoverability scoring
│   │       ├── structure.rs  # L1.5: /StructTreeRoot + MCID (tagged path)
│   │       ├── layout.rs     # L6: XY-cut columns/lines (untagged fallback, RTL order)
│   │       ├── images.rs     # L7: XObject image extraction + filter decode
│   │       ├── tables.rs     # L7: tagged + ruled-line table reconstruction
│   │       └── types.rs      # Glyph, Line, Block(Text/Image/Table), Page, Confidence
│   │
│   └── qalam-py/             # L5: PyO3 bindings, depends on qalam-core
│       ├── Cargo.toml
│       ├── pyproject.toml    # maturin build config
│       └── src/lib.rs
│
├── python/
│   └── qalam/
│       └── __init__.py       # re-exports + any pure-Python conveniences
│
├── cli/                      # optional: `qalam extract file.pdf` for quick testing
│   └── src/main.rs
│
└── tests/
    ├── fixtures/             # real + hand-crafted Arabic PDFs
    │   ├── recoverable/
    │   ├── ligatures/
    │   ├── no_tounicode/     # should be flagged needs_ocr
    │   ├── actualtext/
    │   ├── tagged/           # has /StructTreeRoot → exercises L1.5
    │   ├── multicolumn/      # untagged 2–3 col → L6 reading order (RTL)
    │   ├── images/           # embedded XObject images → L7
    │   └── tables/           # ruled + borderless → L7
    └── expected/             # golden output per fixture (text / blocks / images)
```

Rationale: `qalam-core` knows nothing about Python, so it stays independently usable
(and the CLI reuses it for fast manual testing). `qalam-py` is a thin binding layer.

---

## 6. Milestones

- **M0 — Skeleton.** Cargo workspace builds; `qalam-core` opens a PDF with lopdf and
  lists pages + fonts in `/Resources`. CLI prints the raw content stream of page 1.
- **M1 — Reproduce the bug (on purpose).** Walk `Tj`/`TJ`, resolve codes through
  `ToUnicode`, print the raw (broken) Arabic — reversed presentation forms. *Seeing the
  garbage is the checkpoint.* Track `(x, y, advance)` per glyph here, plus the graphics
  state that gives each glyph its `Style` (fill colour, font, effective size, `Tr` mode).
  Colour costs almost nothing to capture here and cannot be reconstructed later.
- **M2 — Fix it (mind the order).** line grouping by geometry → `unicode-bidi` reorder
  (on presentation forms) → NFKC normalize. The order is not interchangeable (§3, §10).
  Recoverable fixtures now produce correct logical Unicode.
- **M3 — Detector.** Implement L4 confidence scoring; `no_tounicode` fixtures flag
  `needs_ocr` instead of emitting garbage. Add `/ActualText` handling.
- **M4 — Python bindings.** PyO3 + maturin; `pip install -e .` then
  `qalam.extract_text("file.pdf")` works. Return per-page metadata + confidence.
- **M5 — Corpus + benchmarks.** Build a real test corpus, golden-file tests, and a
  benchmark vs pypdfium2/PyMuPDF (correctness parity + speed). *End of Tier A.*
- **M6 — Images (Tier B, do first).** Extract `/XObject` images per page: DCTDecode
  passthrough, Flate+samples → PNG, position via the `Do`-operator CTM. Highest value for
  least risk. Emit `Image` blocks.
- **M7 — Reading order (Tier B).** L1.5 tagged-tree reader + L6 XY-cut fallback with RTL
  column ordering. Emit ordered `Text` blocks; validate against `tagged/` and `multicolumn/`.
  *(L6 landed early, during M3 — see §10.4. The tagged-tree reader and `Block` emission
  are what remain.)*
- **M8 — Tables (Tier C).** Tagged `/Table` structure + ruled-line detection from vector
  graphics. Borderless/alignment inference is a stretch and explicitly best-effort for RTL.
- **M9 — HTML export (Tier D, stretch).** Render the block model to HTML: `dir="rtl"`,
  blocks in reading order, `Span` styling as inline CSS, extracted images inlined. Purely
  additive — it consumes the model, and needs no change to any layer below it.

---

## 7. Testing strategy

- **Golden files.** Each fixture PDF pairs with an `expected/*.txt` of correct logical
  Unicode. Tests assert exact match (recoverable) or `needs_ocr` (unrecoverable).
- **Round-trip validation.** For recoverable pages, re-shape the extracted logical text
  with `rustybuzz` and confirm the shaped glyph sequence matches what the PDF painted.
  This catches subtle ordering/ligature errors that a human diff would miss.
- **Adversarial fixtures.** Hand-craft PDFs with: no ToUnicode; wrong ToUnicode;
  ActualText overrides; mixed Arabic/Latin/digits; lam-alef and other ligatures.
- **Ligature regression (real data).** The `SSTArabic-Bold` line whose CIDs include
  `01BA` (U+FEFB) is a ready-made regression: expected logical output contains `ولا`, and
  the test must fail if it ever produces `وال`. The CID list and golden text are in §10.
- **Cross-check.** Compare against pdfium (pypdfium2) output as a sanity reference —
  it's the closest thing to a correct baseline — but don't treat it as gospel.

---

## 8. Open questions / risks

- **Fonts with no ToUnicode but a standard `/Encoding`** — how far can encoding +
  embedded cmap heuristics get us before we must flag `needs_ocr`? (Measure on corpus.)
- **Mixed-direction lines** (Arabic + Latin + numbers) — verify UAX #9 handling with
  explicit fixtures; this is where bidi bugs hide.
- **Reading order across columns/regions** — now Tier B (L6). XY-cut handles clean 1–3
  column layouts; expect degradation on sidebars, footnotes, headers/footers, and text
  wrapping around figures. Ship with a confidence score, not a correctness claim.
- **Table inference for RTL** — ruled tables are tractable; borderless/alignment-based
  tables are near-greenfield for Arabic and may stay best-effort indefinitely.
- **Image edge cases** — `/SMask` soft-mask transparency (needs merging), JBIG2/CCITT
  scans, and many-fragment images. A single full-page image = "needs OCR" signal, not a figure.
- **Diacritics (tashkeel)** — decide whether to preserve or optionally strip; make it a
  flag, default preserve.
- **Colour spaces beyond Device{Gray,RGB,CMYK}** — `/Separation`, `/ICCBased`,
  `/Indexed` and `/DeviceN` arrive via `cs`/`scn` and need a `/Resources /ColorSpace`
  lookup plus a tint transform to resolve exactly. Plan: handle the three device spaces
  precisely, approximate `/ICCBased` by its `/N` component count (1 -> Gray, 3 -> RGB,
  4 -> CMYK), and record anything else as `Color::Unknown` rather than guessing wrong.
- **Is colour meaningful or incidental?** Body text is near-black in most documents, so
  colour may carry little signal beyond headings and links. Cheap to record, so we do —
  but do not design any *extraction* logic that depends on it.

---

## 9. References (from research)

- Adobe PDF Extract API — Arabic failure thread (presentation forms, reversal).
- pdfminer.six #850, PyMuPDF #2199, pdf.js #2141 — the recurring ligature/order bug.
- lopdf docs (crates.io / docs.rs) — parser foundation.
- `unicode-bidi`, `unicode-normalization`, `rustybuzz` — the reconstruction toolkit.
- ISO 32000 (PDF 1.7 / 2.0) spec — content stream operators, fonts, CMaps.

---

## 10. Findings log — real-file experiment

> A living section. Each entry records something a real document taught us, so the
> design stays grounded in what actually occurs in the wild rather than the spec alone.
> Append freely.

### 10.1 — First end-to-end trace: `SSTArabic-Bold`, an A4 government-style doc

Traced one page all the way from bytes to correct Arabic. The full chain that worked:
`startxref → xref → trailer → Catalog → Pages → Page (obj 19) → /Contents (obj 89) +
/Font C2_0 (obj 68) → /Encoding Identity-H → /ToUnicode (obj 365) → CIDs → presentation
forms → reorder → NFKC → correct logical Arabic.`

**Confirmed decode (three text lines on the page):**
> هذا الدليل إرشادي ولا يغني عن الرجوع إلى الأنظمة واللوائح والقواعد والسياسات والإجراءات ذات الصلة.
> *("This guide is advisory and does not substitute for referring to the relevant
> regulations, bylaws, rules, policies, and procedures.")*

**Concrete lessons (each becomes a rule in the code):**

- **Encoding was `Identity-H`** → codes in the content stream ARE the 2-byte CIDs, no
  intermediate mapping. Split every `Tj`/`TJ` hex string into 4-hex-digit (2-byte) chunks.
  *Do not hardcode this* — read `/Encoding` and branch (simple fonts use 1-byte codes).
- **CID `0003` → U+0020 (space).** A common convention in subset fonts, but it came from
  the ToUnicode map, not an assumption. Always read the space glyph from the map.
- **The ToUnicode map is a MIX.** Same font mapped some CIDs to clean base letters
  (`0113→U+0627` alef) and most to presentation forms (`0114→U+FE8E` alef-final,
  `0164→U+FEDF` lam-initial). This is why NFKC is mandatory, not optional.
- **Ligatures appear as single presentation glyphs.** `01BA→U+FEFB` (lam-alef isolated),
  `01BB→U+FEFC` (lam-alef final). These drove the order-of-operations rule (§3): reorder
  while they're single glyphs, expand after.
- **One CID → multiple codepoints exists.** `01AE→U+0651 U+064B` (shadda + fathatan).
  Proves the map value must be a string. (See §3 key-design bullet.)
- **The "font size 1 + scale in Tm" idiom.** The stream set `/C2_0 1 Tf` then
  `20.5559 0 0 20.5559 118.66 476.77 Tm`. Effective size ≈ 20.56pt, not 1pt. **Position
  and size = compose Tf-size × Tm × CTM.** Reading `Tf` alone is a bug.
- **Most of the content stream is graphics noise.** Before `BT` the stream drew a
  rounded-rectangle banner with `re/m/l/c/f/S/W/n/cm/rg/RG/g/gs/q/Q`. Text lives ONLY
  between `BT`/`ET`; the interpreter tracks state everywhere but emits only inside text
  objects.
- **Marked content wraps the text** (`/OC BMC … EMC`). Here `/OC` = an optional-content
  layer (no `/ActualText`), so nothing to override. But the bracketing is where
  `/ActualText` *would* live — L2 must watch `BDC`/`BMC` properties for it.
- **`/Length` is an indirect object** (`/Length 90 0 R`, `/Length 366 0 R`). The writer
  back-patches stream length. lopdf resolves this, but a hand-rolled parser must.
- **Page geometry:** MediaBox `595.276 × 841.89` = A4 (210×297mm at 72dpi), origin
  bottom-left; `/Rotate 0`. Handle non-zero `/Rotate` in the coordinate transform.
- **`/DescendantFonts` (obj 364)** holds glyph widths/outlines — needed later for glyph
  *advances* (positions), not for text *meaning*.

**Frozen regression case** (for `tests/fixtures/ligatures/`):
- CIDs (line 1, as painted, visual order):
  `0003 0179 0164 010D 0003 014F 0175 0124 0134 0164 0113 0003 016E 0150 0003 017D 016D
  0155 017B 0003 01BA 0174 0003 017A 012F 0114 013C 0133 010D 0003 0166 017C 0164 0130
  0164 0113 0003 0113 0132 0171`
- Expected logical output: `هذا الدليل إرشادي ولا يغني عن الرجوع إلى`
- Must-not-produce: any output containing `وال` in place of `ولا`.

### 10.2 — ToUnicode CMap grammar (what `font.rs` must parse)

The `/ToUnicode` stream is a PostScript-flavoured CMap. The parser only needs a few tokens:
- `begincodespacerange … endcodespacerange` — code width. `<0000> <FFFF>` = 2-byte codes.
- `beginbfchar … endbfchar` — single mappings: `<srcCode> <dstUTF16BE>` per line.
- `beginbfrange … endbfrange` — range mappings (not seen in this file but common):
  `<lo> <hi> <dstStart>` or `<lo> <hi> [<dst0> <dst1> …]`. Must be supported.
- Destinations are **UTF-16BE**, possibly multiple code units → decode to a `String`.
It is NOT full PostScript; a small tokenizer over these keywords is enough. Do not pull in
a PostScript interpreter.

### 10.4 — Columns are not optional, and a global column pass cannot find them

Page 6 of the test file lays three "cards" side by side under a full-width intro
paragraph. Grouping glyphs by baseline — correct on every single-column page — took one
fragment from each card per line and interleaved three separate paragraphs into nonsense,
*even though every glyph decoded perfectly*. Correct characters, unreadable text. This is
why L6 was pulled forward from M7 into M3: multi-column layout does not degrade output, it
corrupts it.

Measured on that page:

```
whole page   widest empty band: 116pt horizontal  → intro above, cards below
card band    widest empty band:  51pt vertical    → right card | rest
the pair     widest empty band:  43pt vertical    → middle card | left card
```

**The load-bearing finding: the page has no page-wide vertical gutter at all.** The
full-width intro paragraph crosses all three columns, so an x-projection over the whole
page finds nothing. A single global column-detection pass is not merely less accurate here
— it detects zero columns. Only the *recursion*, having first cut the intro away on the
horizontal gap, exposes the gutters underneath. Hence recursive XY-cut splitting at the
**widest** band each time, rather than at every band at once.

Rules this produced in `layout.rs`:
- **Split at the widest gap, then recurse.** Choosing between a horizontal and a vertical
  cut by which band is wider is what lets a full-width heading be removed before the
  columns beneath it are looked for. No explicit "is this a heading" rule is needed.
- **Thresholds scale with the region's median type size**, not absolutes. A 20pt gap is a
  column gutter in 9pt text and ordinary word spacing at 40pt.
- **Never column-split a region less than ~2 lines tall.** A table-of-contents line is one
  line with a large hole between title and page number; without this guard the hole reads
  as a gutter and tears the line in half.
- **Median, not mean, type size** — one 40pt heading among 500 words of body text must not
  move the thresholds.
- Reading order: rows always top-to-bottom (PDF `y` grows *upwards*, so the higher band is
  read first); columns right-to-left when the page is RTL. That direction is decided per
  *page*, not per line, so a column of Latin figures cannot reverse the page's column order.

### 10.5 — Tashkeel: two separate bugs, both silent

The word `تتضمّن` on page 5 extracted as `تتض َّمن` — a space inside the word. Two
independent causes, and the same file contained two more words (`تصورًا`, `مبدئيًّا`) that
were corrupted by the second cause alone while still *looking* plausible.

1. **NFKC supplies the space itself.** `NFKC(U+FC60)` — the isolated shadda-with-fatha
   ligature — is `SPACE + FATHA + SHADDA`. Unicode gives an *isolated* mark a space to
   render on. In extracted text the mark is never isolated; it belongs to the letter beside
   it. So a space immediately followed by a combining mark must be dropped.
2. **Marks land before their base after reordering.** UAX #9 rule L3 says combining marks
   *precede* their base once reordered, and renderers swap them back for display. Going
   visual→logical we hit the inverse. The mark is painted at an `x` **inside** its base
   letter's span (mark at 377.4 within the meem's 374.1–381.1), so sorting by x puts it
   after the meem and the reversal puts it before. Marks must therefore be emitted *before*
   their base in the visual string, so the reversal lands them after it.

**Detecting a mark cannot be done on the raw character.** U+FC60 is `Lo` (a letter) by
category and carries a non-zero advance of 1.353, so neither a category test nor a
zero-advance test finds it. The reliable test is what it *normalises to*: strip a leading
space, and ask whether everything left is a combining mark.

### 10.3 — Structure is reconstructed, not read (untagged test file)

The `SSTArabic-Bold` document showed no `/StructTreeRoot` in the objects we inspected and
used `/OC` optional-content layers rather than structure tags — i.e. it looks **untagged**.
So it exercises the *reconstruction* path (L6), not the tagged path (L1.5). We confirmed the
geometric approach is viable: a synthetic 2-column RTL page was correctly ordered
right-column-then-left, top-to-bottom, and right-to-left within each line, using XY-cut
gutter detection on word bounding boxes alone — no structure hints needed. Takeaway: treat
the tagged path as a lucky bonus and make the geometric fallback the workhorse. (Caveat:
one sample; confirm on more files before assuming most target docs are untagged.)
