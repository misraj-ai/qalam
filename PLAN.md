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
  `/ToUnicode` → simple-font `/Encoding` (+ `/Differences`) → embedded font's own charset.
  With one exception: when `/ToUnicode` and `/Differences` disagree about a **numeric
  separator**, the latter wins, because the renderer checks it and nothing checks
  `/ToUnicode` (§10.15). The override is confined to separators precisely so that it cannot
  become a general licence to second-guess the map.
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
│       ├── Cargo.toml        # [lib] name = "qalam", crate-type = ["cdylib"]
│       ├── pyproject.toml    # maturin build config
│       ├── README.md         # the package's PyPI front page
│       └── src/lib.rs
│
├── cli/                      # optional: `qalam extract file.pdf` for quick testing
│   └── src/main.rs
│
├── scripts/
│   └── compare.py            # correctness + speed vs pypdfium2 / PyMuPDF
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
- **M4 — Python bindings.** *(done)* PyO3 + maturin; `maturin develop` then
  `qalam.extract_text("file.pdf")` works, plus `Document`/`Page`/`Line` with per-page
  metadata and confidence. Ships as an abi3 wheel (one wheel for Python 3.9+).
- **M5 — Corpus + benchmarks.** *(partly done)* Golden-file tests
  (`crates/qalam-core/tests/golden.rs`, regenerate with `UPDATE_GOLDEN=1`) and a
  correctness+speed comparison against pypdfium2/PyMuPDF (`scripts/compare.py`) are in
  place — see §10.7. The corpus is now **two** documents, and the second immediately paid
  for itself by exposing a whole class of ligature bug (§10.10) that the first could not
  reach. Still far short of a corpus: every threshold in `detect.rs`, `layout.rs` and
  `tables.rs` remains a judgement call, and more fixtures is still the highest-value work
  available.
- **M6 — Images (Tier B, do first).** *(mostly done)* Extract `/XObject` images per page:
  DCTDecode passthrough, Flate+samples → PNG, `/Indexed` palettes expanded, positioned by
  the `Do`-operator CTM, emitted as `Image` blocks — see §10.8.
  **Still to do:** images nested inside form XObjects, `/SMask` compositing, and sub-byte
  sample depths.
- **M7 — Reading order (Tier B).** *(done, one caveat)* L6's recursive XY-cut landed early
  during M3 (§10.4); the L1.5 tagged-tree reader and the `Block` model followed. A tagged
  page's reading order now comes from its `/StructTreeRoot`, everything else from geometry.
  **Caveat:** neither corpus document is tagged, so the tagged path is proven only against
  PDFs the test suite builds byte by byte (§10.9). Structure tags are also not yet used for
  block *types* — an `/H1` is still just a short text block.
- **M8 — Tables (Tier C).** *(ruled path done)* Ruled-line detection from vector graphics:
  the interpreter now reports painted axis-aligned rules, `tables.rs` clusters them into a
  grid, and cells are filled with the text inside them, columns numbered right-to-left —
  see §10.11. 18 tables found in `bar_Persons.pdf`, none invented in the other fixture.
  Rotated column headers read correctly (§10.14), and **borderless tables are inferred from
  alignment** (§10.23) — 28 of them in `12.pdf`, none invented across the rest of the corpus.
  **Still to do:** the tagged `/Table`/`/TR`/`/TD` path — `bar_Persons.pdf` tags 21 tables and
  1,709 cells, so the information is sitting there — and column spans, which currently cut a
  spanning header mid-word.
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
- **Type1 and TrueType font programs are not read.** `cff.rs` handles `/FontFile3` only.
  `/FontFile` (Type 1) hides its encoding in an eexec-encrypted section and `/FontFile2`
  (TrueType) names glyphs in a `post` table; `bar_Persons.pdf` embeds seven Type 1 fonts, so
  this is not hypothetical. Both are additive work behind the same interface.
- **Digits are reported as `/ToUnicode` claims, not as the font names them.** The same fonts
  that mis-map separators also map Arabic-Indic digit glyphs to Latin characters, so a page
  rendering `٣٫٧٠٩` extracts as `3٫709`. The value is right and the ordering is right;
  only the script is the map's rather than the font's. Extending the arbitration to digits
  would make it fully faithful at the cost of changing a great deal of output — a decision
  worth taking against a real corpus rather than one document.
- **Only quarter turns are recognised.** `TextOrientation` snaps to the nearest 90°, so text
  set on an arbitrary angle — a diagonal watermark, a fan of labels round a pie chart — is
  treated as horizontal and will group badly.
- **Kashida (tatweel) justification.** `bar_Persons.pdf` stretches words to the margin by
  inserting U+0640 between letters, so `المعظم` is stored as `المعظــم`. We extract it
  faithfully, which is right — it is in the file — but it means extracted text will not
  match a search for the normally-typed word. Stripping it should be a flag alongside the
  tashkeel one, defaulting to preserve.
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

### 10.24 — A table row is a band, not a baseline

`21.pdf` and `22.pdf` are the same document twice: one draws its table borders, the other does
not. The ruled one gave a perfect 56x14 table on every page. The borderless one gave
**nothing at all**, on any of its 41 pages — even after §10.23.

The cause was two lines of arithmetic, not a missing idea. `22.pdf` sets its row numbers about
**3pt below** the rest of their row, and the row grouping keys on the baseline. So every row
split in two: a wide piece holding the record, and a 8pt-wide piece holding `55`. That halved
nothing and doubled everything — 73 rows where there are 36 — and, worse, half of them now
covered a sliver of the page, so no column boundary could gather the support §10.23 requires.

**A table row is a horizontal band.** The cells in it need not share a baseline: a different
font, a different script, or a smaller size shifts one by a point or two. The test that makes
rejoining them safe is **horizontal disjointness** — two pieces of text at the same x cannot
be one row however close their baselines, while two at different x, a fraction of a line
apart, are one row in different columns. Consecutive lines of a paragraph overlap in x almost
entirely, so they are never merged.

A second correction fell out of the same file. A band reaches as far as the row spacing stays
regular, which on a page whose table fills it is further than the table: `22.pdf` picked up the
letterhead above and the notes below, and chopped them into cells that split words. Trimming
the *ends* back to rows that honour the boundaries removes the notes, but not the letterhead —
it sits in two corners with a wide gap between, so it honours every boundary while looking
nothing like a row. What gives it away is **occupancy**: two columns out of eight. Only the
ends are trimmed, because an interior row that spans — a section heading inside a financial
statement — belongs to the table.

Result: 41 tables across 41 pages of `22.pdf`, 10 across 10 of `21.pdf`, and the two files now
agree. It also removed the mangled header from `12.pdf` noted in §10.23 — `ريا` / `ل سعودي`
was a spanning row that the trim now excludes, so the currency labels come back as ordinary
text instead of split cells.

### 10.23 — A borderless table is one that no single projection can see

`12.pdf` is a financial report whose tables are drawn with **no lines at all** — page 12 holds
six columns held together by alignment alone. The page-level cut separated three of them and
merged the rest, so a whole row of figures arrived as one line: `1,653,281 1,637,299 24`.

Reconstruction reuses what the ruled path already has — `Grid`, `fill`, `is_plausible` — and
adds two things.

**The rows have to vote.** The obvious approach is the one `layout.rs` uses: project every
item onto x and take the empty bands. It fails here, and not marginally. Page 12 carries a
title above the table, a footer below it and section headings inside it, each spanning the
full width; projected together with the body they leave **one** gap where there are five. So
each row votes only over its own extent, and a position becomes a boundary when most of the
rows crossing it leave it clear. One row that ignores the columns then cannot hide them.

**A cell is re-read from the glyphs, not from the lines.** A ruled table can be filled from
the lines the pipeline already built, because its cell walls also split those lines. An
inferred grid has no walls: the page-level cut never saw its boundaries, so one line runs
across several cells. `arabic::text_within` re-runs the full pipeline over each cell's glyphs
— overlays anchored, order resolved, marks placed, NFKC applied — which is what keeps a cell's
Arabic as correct as a paragraph's.

**Where the line is drawn, and why there.** Page 8 of `test_for_arabic_barser.pdf` sets two
columns of numbered cards. The badges form a third column between them, every row shares a
baseline, and the cells are even about the right length. It is **geometrically
indistinguishable** from a table; what separates them is that one is wrapped prose continuing
from row to row, which is a fact about language and not about where the ink is. So the bar
sits at **four** columns, where geometry can still carry it: each extra aligned column
multiplies the improbability of coincidence, and four columns of prose is rare where a
four-column table is ordinary.

The cost is stated rather than hidden: a genuine three-column borderless table is missed. That
is the same trade `is_plausible` makes for ruled grids (§10.11) — a false table destroys text,
a missed one merely leaves it unstructured. Measured: 28 tables found in `12.pdf`, **none
invented** in the other five documents.

Known limitation: column spans are not modelled, here or for ruled tables. A spanning header
row is trimmed off the table (§10.24) and emitted as ordinary text rather than split into
cells, which loses the association but not the words.

### 10.22 — `Tc` is spacing between glyphs, not part of one

Pages 4–11 of `12.pdf` came back one character at a time:

```
  before:  ت ق ر ی ر ا ل م ر ا ج ع ا ل م س ت ق ل
  after:   تقریر المراجع المستقل
```

The document sets `Tf 1` with **`Tc = -0.75`**, compensating with large `TJ` kerns. The
positions were right all along — but the *reported* advance included `Tc`, so it came out at
about **-4pt per glyph**. A negative advance made the running edge walk backwards, every next
letter looked far away, and the word-gap rule fired between every pair.

`Tc` and `Tw` move the pen *between* glyphs; they are not part of any glyph. An advance that
includes them stops being a width — and a width that can go negative corrupts everything built
on it: bounding boxes, layout items, table cell containment, overlay detection. `Glyph::advance`
now reports the glyph's own ink extent, `width x Tfs x Th`, which cannot be negative. The full
pen displacement stays inside the interpreter, where it is the right quantity.

It repaired things elsewhere that had looked like separate faults: in `1.pdf`, `قيلل` became
**`يقلل`** — the correct word — and two spaces lost to an earlier change came back.

### 10.21 — A hamza painted over its letter is not a second letter

`8.pdf` doubled every hamza: `والإدارية` came out `والإإدارية`, `الأدبية` as `الأأدبية` —
**277 times** across the document.

The font paints `إ` as a **zero-advance overlay at the same x** as the glyph carrying the
letter it belongs to. Both carry Unicode, so both were emitted. The overlay is not an extra
letter; it says the alef already there wears a hamza.

Two shapes, and the second is what made the first fix look like it had failed:

- **The host holds the bare letter.** A `لا` ligature with an `إ` painted on it means `لإ`.
  The host's alef is replaced by the composed character.
- **The host already holds the composed letter.** The same font *also* supplies a `لإ`
  ligature and paints the hamza over that too. There the overlay adds nothing at all and only
  the host survives. A first attempt handled only the first shape, and the doubling persisted
  in exactly the places where the font had been most helpful.

Which letters compose is asked of **Unicode**, not tabulated: `إ` is canonically `ا` plus a
hamza below, `ؤ` is `و` plus a hamza above, and so the rule covers whatever the standard says
without a table to fall out of date.

**Where it declines to act.** If the host contains the base letter more than once — `الا` with
one hamza — nothing says which wears it, and putting it on the wrong letter would be worse
than leaving the overlay separate. That case returns `None` and the text keeps both, visibly
odd rather than quietly wrong.

This is the third distinct thing `8.pdf` does with zero-advance glyphs, after §10.18's overlay
letters and §10.20's meaningless ones. The pattern is now hard to miss: **a glyph that does
not move the pen is a modifier of its neighbour, and the only question is what kind.**

### 10.20 — The same character can mean "no text" or "lost text"

`8.pdf` produced hundreds of U+FFFD — 194 on page 5 alone — scattered inside otherwise
perfect words: `التشريعية` came out `الت�شريعية`, `المرسوم` as `المر�سوم`.

U+FFFD is *our* marker for a code we could not resolve, so the obvious reading was that we
were failing. We were not. The font's own `/ToUnicode` says:

```
  <B0> <0634>     ش
  <FB> <FFFD>     the rest of it
```

The document renders several Arabic letters in two pieces and **maps the second piece to the
replacement character on purpose**. That is the producer stating the glyph carries no text —
the exact opposite of what our own U+FFFD means, and we had been conflating the two.

The distinction was already available and unused: `fonts.decode` returns `None` when nothing
could resolve the code, and `Some("\u{FFFD}")` when a source resolved it *to* the replacement
character. So:

- **`None` — lost text.** Emit U+FFFD and count it. Dropping it would turn "we cannot read
  this" into "there was nothing here", which is the deception the project exists to prevent.
- **`Some(U+FFFD)` — ink, not text.** Drop it, and do not count it. The glyph still advances
  the pen, so the running edge is updated; it simply contributes no characters.

**Removing it also repaired the reading order**, which was the surprise. `يؤكد` had been
extracting as `ي�كؤد`, with the `ؤ` and `ك` transposed around the intruder — so the stray
character had looked like two separate faults. It was one.

Result across the document: hundreds of replacement characters down to six, and pages that had
been reported `degraded` are now `ok` — correctly, because the text really is complete. The
six that remain are cases where the font leaves a letter genuinely unreadable, and those still
carry the marker, which is what they should do.

**The general rule.** "Always strip U+FFFD" would be wrong; it would hide real losses. What
makes dropping safe here is not the character but *who wrote it*: a producer declaring a glyph
meaningless is evidence, and evidence from the file always beats a rule of thumb applied to
its output.

### 10.19 — How wide a gutter must be depends on how tall it is

Every two-column page of `9.pdf` — an 80-page magazine — merged its columns line by line, so
each extracted line held half a sentence from each. The gutter measured **14pt** against 11pt
type: 1.27 em, against the 1.5 em `MIN_GUTTER_EMS` demanded. Two and a half points.

Raising the number would have been guessing. What the projection actually reports is stronger
than a width: a gap in the x-projection means **no glyph anywhere in the region** occupies
that band — so in a region of many lines it is not a word space that happened to be wide, it
is a band every single line agreed to leave empty. The taller the region, the less plausible
that is as coincidence, and the less the width has to prove alone. Above about five lines the
requirement drops to 0.9 em; below it the strict width stands, because a short region really
could align a few wide spaces by chance.

That alone over-corrected. Page 14 of `test_for_arabic_barser.pdf` runs a ring of numbered
badges down the inside edge of each column, and the projection sees a perfectly good gutter
beside them — so every number was torn from the item it numbered. Hence a second rule:
**both sides of a column cut must be wide enough to hold words** (5 em). A narrower strip is
furniture — a badge, a bullet, a margin rule — not a column.

Two things worth recording about the result:

- **The relaxed threshold found real structure the old one had missed.** On page 14 the
  lettered labels `أ ب ج د ه` had been emitted as a block of bare letters followed by a block
  of unlabelled sentences; they now sit with their text. The page had been wrong in the golden
  file all along, and looked plausible enough that nobody noticed.
- **Dense newspaper pages remain mixed.** `6.pdf` sets columns with gutters of 0.9–1.3 em
  interrupted by spanning headlines and images; its widest internal band is 1.3 em and the
  layout is genuinely ambiguous from geometry. Checked and confirmed *not* a duplication bug:
  11,295 glyphs, no form XObjects, three coincidental overlaps. This is the degradation
  PLAN.md §8 predicts for sidebars and wrapped figures, and it is honest to leave it.

### 10.18 — A position is not always a place in the sequence

Page 3 of `3.pdf` draws the `ز` of `ميزات` and the `ر` of `المشروع` with **zero advance**,
positioned inside the neighbouring glyph's ink and nearly 4pt above the baseline. Sorted by
their own coordinates they became `م زيات` and `المرشوع`, and one was thrown onto a line of
its own:

```
  before:  زر
           الغرض من هذا المستند هو تحديد مي ات المشوع، …
  after:   الغرض من هذا المستند هو تحديد ميزات المشروع، …
```

**The file is not corrupt, and this is worth stating because it looks like it is.** Reversing
the painting order recovers `المشروع` and `ميزات` exactly; the information is all there. Even
the producing viewer's own copy-and-paste gets it wrong, which is evidence about the viewer's
heuristic rather than about the file.

Ordering by geometry rests on an assumption that is usually invisible: *a glyph's coordinates
say where it comes in the text*. That holds for a glyph which **advances the pen** — the
arithmetic put it after its predecessor. A zero-advance glyph moves nothing, so its
coordinates say only where the ink landed, which can be anywhere.

The rule that came out of it — and three attempts before it, each rejected by a fixture:

- **Anchor on containment, not on painting adjacency.** A zero-advance glyph drawn *inside*
  another glyph's ink inherits that glyph's sequence position. Anchoring every zero-advance
  glyph to its painting neighbour instead moved the full stops in `1.pdf` to the front of
  their lines: punctuation that merely lacks an advance still sits where it belongs.
- **Combining marks are excluded.** A mark is also zero-advance and also drawn over its base,
  but the existing mark handling (§10.5) already places it by reading its own position.
  Anchoring turned `تصورًا` into `تصوراً` — and painting order cannot separate the two cases,
  because `test_for_arabic_barser.pdf` paints its marks *before* the letter they sit on while
  `3.pdf` paints its overlays *after*. What the glyph **is** decides it, not where it came.
- **"Does not move the pen" and "is a combining mark" are different questions.** They had been
  one predicate, which is what made a zero-advance `ز` get treated as a mark and moved to the
  wrong side of its neighbour.
- **A space drawn on top of a letter is not a space.** It separates nothing. Gated on the same
  containment test, because a narrow space that merely lacks an advance is still a real gap —
  dropping those cost `1.pdf` the space after a colon.

Net effect on `1.pdf`, which needed none of this to be readable: `ف( رنون` → `(فرنون`,
`م / عدل التحويل` → `/ معدل التحويل`, `و تسويق` → `وتسويق`, `ب ها` → `بها`.

### 10.17 — The text state belongs to the graphics state, and `Q` restores it

Fixture `1.pdf` extracted its opening line as

```
  الشركةع البرو طةين– ات رنساشن وايلن        (ours)
  الشركة عبر الوطنية – ترانس ناشيونال       (correct)
```

Every character present, every character correct, only the **order** wrong — and wrong in a
way that reads as a hopeless extractor rather than a single bug. Some lines on the same page
were perfect.

The document sets `-4.02 Tc` inside a `q … Q` block, eleven times. ISO 32000 §8.4.1 puts the
text state *parameters* — `Tc`, `Tw`, `Tz`, `TL`, `Tf`/`Tfs`, `Tr`, `Ts` — in the **graphics**
state, so `q` saves them and `Q` restores them. We kept them in the text interpreter as
globals that nothing restored, so a spacing set for one four-glyph run leaked over the rest of
the page.

Then: every subsequent glyph advanced 4pt too little, the computed x positions collapsed into
each other, and the geometric sort that decides reading order shuffled the glyphs. The cause
was in `graphics.rs`; the symptom appeared in `arabic.rs`, two layers away.

Rules this produced:

- **Where state lives is the spec's decision, not a convenience.** Grouping everything
  text-related in the text interpreter was tidy and wrong. The text *matrices* really are
  outside the graphics state — `BT` creates them and `ET` destroys them — and that is the only
  part of the text state that is.
- **A wrong advance is a wrong reading order.** Positions are ground truth for order (§3), so
  anything that corrupts an advance corrupts the text itself, not merely its spacing. That
  makes advance arithmetic worth the same care as the CMap.

Diagnosing it took a detour worth recording: the glyph widths were the obvious suspect and
were *correct* — `w_em = 0.451` for a letter whose emitted advance was 2.75 at 15pt. Solving
`advance = 15·w_em + b` across several glyphs gave `b = -4.02` exactly, which named the
operator. Fitting the observation rather than guessing at causes is what found it.

### 10.16 — Form XObjects hide text, and their resources are scoped

Page 1 of `test_for_arabic_barser.pdf` was missing its title. A form XObject is a page within
a page: its own content stream, its own `/Resources`, its own fonts under its own names. Text
inside one is invisible to an interpreter that does not descend into it, and **nothing in the
outer stream hints that anything was missed** — the extraction simply looks complete.

Two things the recursion needed beyond walking into the stream:

- **Resource names are scoped.** A page and a form may both call a font `C2_0` and mean
  different fonts, so names are qualified by nesting (`Fm1/C2_0`). A form that omits
  `/Resources` inherits the enclosing ones, and following that inheritance is what makes its
  text decode at all.
- **The font *summary* list matters as much as the font map.** The interpreter consults
  `&[FontInfo]` to decide 1-byte versus 2-byte codes. After the recursion worked, the title
  still came out as replacement characters, because the summaries held only page-level names:
  `Fm1/C2_0` was not found, defaulted to single-byte, and **split every 2-byte CID in half**.

It also caused a regression the golden file caught. The forms drew one short decorative rule
near the foot of a page that happened to overlap a table horizontally; `group_rows` chained on
x-overlap alone, swallowed it, and stretched the group from 147pt tall to 463pt — after which
no real column cleared the "at least half the table's height" filter and the table vanished.
Row spacing is now judged against the **whole page's** median gap, because judging
incrementally cannot work: the first two boundaries of a group have nothing to compare
against. That turned out to improve detection as well as repair it — 21 tables found where
there had been 18, and page 6's confidence rose from 0.79 to 0.96.

### 10.15 — Which of a PDF's own claims to believe

Three of the day's bugs came down to one question: when a file contradicts itself, which
part of it is telling the truth? The answer turns out to be structural rather than a matter
of taste.

**Trust what the renderer checks.** `/ToUnicode` exists solely to help text extraction —
*nothing draws it*. A producer can write it wrong and every page still looks perfect, so the
error survives proofreading, printing and publication. `/Encoding /Differences` is the
opposite: the renderer walks code → glyph name → outline through it, so an error there is
visible on the page and gets fixed before anyone ships.

That asymmetry is decisive, and `bar_Persons.pdf` demonstrates it three times over:

```
                       /ToUnicode says   /Differences says   the page shows
  code 161 (T1_0)      .                 uni066B  ٫          ٫
  code 131 (T1_0)      7                 uni0667  ٧          ٧
  code 159 (T1_0)      6                 uni0666  ٦          ٦
```

The font contains **no Latin digits at all** — its CFF charset holds only `uni0660`–`uni0669`
and the Arabic separators. `/ToUnicode` was describing glyphs that do not exist in it.

So `font.rs` now arbitrates. **Narrowly**, because preferring glyph names wholesale would be
reckless — plenty of subset fonts name glyphs `g42` or `cid123`, and there a good
`/ToUnicode` is the only real information available. A correction is made only when both
sources give exactly one character, both are numeric separators, and they differ. It can
therefore **only ever turn one separator into another**: never a letter, never a digit,
never a ligature. On a document whose sources agree — every correct PDF — nothing is
corrected at all, which the first fixture's byte-identical golden file proves empirically.

What it repairs is worth the care: `3,709` was extracting as `3.709`, a value wrong by a
factor of a thousand and wrong in a way no reader would catch. It now reads `3٫709`, which
is what the font says and, just as importantly, **cannot be silently misparsed** by whatever
consumes it downstream.

`cff.rs` reads the same names from the embedded font program as a further fallback, and is
the fourth rung of §3's chain. It was built first, before the discovery that `/Differences`
already had the answer — a reminder to exhaust the PDF's own dictionaries before parsing
binary font programs.

**What remains unfixable from inside the file.** Once corrected, some pages group one number
with `٫` and another with `,`. The document contradicts *itself*, and no amount of parsing
settles that, so L4 flags the page `degraded` with a reason. That is the honest end of the
line.

### 10.14 — Text does not always run left to right

A table's narrow column headers are routinely set on their side to fit. Page 8 of
`bar_Persons.pdf` does exactly that, and every layer of ours assumed text advances along x:
each glyph landed on its own baseline, so `أقل من ١٥` came back as `أ( ق ل م ن ٥١ )`.

The direction is taken from the **text rendering matrix** — specifically where it sends the
unit x vector — rather than inferred from where glyphs happen to fall. A `Glyph` carries a
`TextOrientation`, and exposes `along()` and `across()`: its position along the reading
direction and across it. Line grouping, word-gap detection, mark attachment, bounding boxes
and the layout items all work in those terms, so one code path serves any quarter turn. A
change of orientation also ends a line, however close the glyphs are.

### 10.13 — Numbers are left-to-right, and a glyph may be a whole number

Two bugs in one place, and the first was ours.

**A glyph can decode to several digits.** `bar_Persons.pdf` has one whose `/ToUnicode` value
is the three characters `201` — a single glyph for a year's leading digits. The
multi-character pre-reversal added for Arabic ligatures (§10.10) reversed it to `102`, so
`(2016 - 2017)` came back as `(1026 - 1027)`. The rule was right for ligatures and wrong in
general; it now asks whether a piece contains a strong right-to-left **letter**, and digits
of both scripts are explicitly excluded. Nothing about this is visible in the output — it is
simply the wrong number, which is the worst way to be wrong.

**A number split across two digit scripts is not merely ugly.** Fonts here map most digit
glyphs to one script and a few to the other, so `2017` arrives as `20١7`. Latin digits are
bidi class EN and Arabic-Indic ones AN, so a mixed number is three runs rather than one and
the reorder moves them independently — the digits end up in the wrong *order*, not just the
wrong script. `unify_digit_runs` unifies each run to its majority script. That is a repair
rather than a guess: **a number cannot be written in two numeral systems at once**, so a run
that appears to be is certainly a mapping fault.

### 10.12 — A synthetic fixture proves a reader runs, not that it is right

`bar_Persons.pdf` **is tagged** — `/StructTreeRoot`, `/MarkInfo /Marked true`, and all 36
content pages took the tagged reading-order path built in §10.9. The output was far worse
than geometry would have produced:

```
before:  3 / م هــم ذوي / 2017 / م و / 2016 / ) أن حــوالي ربــع...   six fragments
after:   يوضــح الجــدول رقــم (3) أن حــوالي ربــع المســجلين...      one sentence
```

The cause: **3,638 `/Span` elements**. ISO 32000 §14.8.4 divides structure types into
*block-level* elements that stack down the page and *inline-level* ones that flow within a
line. `/Span` is the common inline type, and producers wrap one around every number and
every change of styling. Treating each as a block gave every inline figure its own
paragraph.

Two rules followed:

- **Only block-level tags open a region**; inline tags (`Span`, `Link`, `Quote`,
  `Reference`, …) join the block they sit in. An *unrecognised* tag is treated as
  block-level, because a custom name is far more likely to be a paragraph style than an
  inline span, and gluing unrelated paragraphs together is worse than splitting them.
- **`/RoleMap` must be read.** This file's tags are `NormalParagraphStyle`, `Story`,
  `Paragraph_Style_1`, `ara_table_text` — InDesign's own names, meaningless until the map
  translates them to standard types.

The lesson generalises past tagging: a fixture we write ourselves can only test the
behaviour we already thought of. It proved the reader parsed a tree and honoured its order —
both true, and both beside the point.

### 10.11 — Tables: geometry finds the grid, but only the text can confirm it

`bar_Persons.pdf` yields 18 ruled tables. Page 5's comes out essentially perfect — seven
columns of governorate statistics, `المحافظات` as column 0 because the page is RTL.

Two rules did the work:

- **A border is drawn one of two ways, and both must be read.** A stroked segment
  (`m`/`l`/`S`) *or* a filled rectangle so thin it reads as a line (`re`/`f`). The second
  is at least as common, and an implementation looking only for strokes misses half the
  tables in the world. Thin filled rects are collapsed to their centre line so everything
  downstream sees one representation.
- **`re W n` paints nothing.** Every page in the first corpus opens with a full-page
  clipping rectangle; treating path *construction* as painting would draw a border around
  all 44 of them.

**The finding that matters: detection cannot be done on geometry alone.** Page 4 of the
first corpus draws a rounded frame **twice**, one offset behind the other as a shadow —
four horizontal rules and four vertical, every one spanning the full extent. That is
geometrically indistinguishable from a 3x3 grid, and reading it as one shredded a
paragraph into empty cells. The golden file caught it; nothing else would have.

What separates a table from a frame is not where the lines are but **what is inside**: a
real table fills most of its cells, a frame has everything in the middle one and nothing
elsewhere. So the rejection lives in `Table::is_plausible`, *after* the text has been
placed — not in `detect`, which genuinely cannot know. A false table is worse than a
missed one: missing one leaves text merely unstructured, inventing one destroys it.

Open: rotated header cells. Narrow headers are set vertically, so every glyph becomes its
own line and `أقل من (١٥)` comes back as `أ( ق ل م ن ٥١ )`. Cell lines are sorted for
reading order, which makes the result deterministic but not correct.

### 10.10 — A second class of ligature, and why one corpus could not find it

`bar_Persons.pdf` uses `Type1` simple fonts, and 14 of its glyph codes map through
`/ToUnicode` to **several base letters at once**: `لم`, `لج`, `بح`, `في`, `هم`, `لله`. One
glyph, several letters, already in reading order.

Our line-level reversal was reaching inside them, so `المعظم` came out `املعظم` and
`بحياة` came out `حبياة`. PyMuPDF has the same bug on this file.

This is §10.1's rule generalised, and the generalisation is the lesson:

> A multi-character `/ToUnicode` value is an **atom of logical text**. The visual-to-logical
> reversal must not reach inside it.

The first corpus could not have found this. Its ligatures mapped to *single* presentation
forms — `ﻻ` is one character until NFKC expands it, and NFKC runs after the reorder, so the
order-of-operations rule protected them for free. Here the CMap hands back base letters
directly; there is nothing left to defer, and the ordering has to be right at the reversal
step. `/ActualText` had already needed exactly this treatment, so the fix was to stop
special-casing it and apply the rule to every piece.

**Residual, and not ours:** some words come back short — `القوانــن` for `القوانين`. PyMuPDF
produces the same, so the file's own `/ToUnicode` is lossy. Guessing the missing letters
would be exactly the invention this project refuses.

### 10.9 — Tagged PDFs, tested against bytes we wrote ourselves

> **Superseded in part — see §10.12.** When this was written the corpus held one untagged
> document, so the reader was built against PDFs the test suite writes itself. The second
> fixture turned out to be tagged, and promptly showed that a synthetic fixture proves a
> reader *runs*, not that it is *right*.

`test_for_arabic_barser.pdf` is untagged: `/StructTreeRoot` appears **zero** times in it.
Rather than ship a reader that had never seen real input, the test suite builds tagged PDFs
from scratch — real indirect objects, a real xref table, parsed by the same `lopdf` the
library uses. Weaker evidence than a document from InDesign, far stronger than asserting
against hand-made structs.

The fixture is built so **structure order and geometry disagree**: `Alpha` painted low,
`Beta` above it. Geometry reads `Beta` first; the tree says `Alpha`. That premise is
*asserted*, not assumed — if the geometric path ever started agreeing, the tagged tests
would pass for the wrong reason and nobody would notice.

Rules this produced:
- **Everything degrades to "no opinion", never to an error.** A tree can cover part of a
  page, name marked-content ids the content stream never defines, or exist without the
  `/Marked` flag. All are ordinary; none deserves a warning. `ReadingOrder::from_structure`
  returns `None` and geometry takes over silently.
- **A tree covering under 80% of a page is refused.** Ordering a tagged fragment
  confidently and appending the rest reads *worse* than ordering the whole page
  geometrically.
- **`/Pg` is inherited down the tree**, like the page tree's own inheritable attributes.
- `cargo test` runs in parallel: two tests writing one temp path produced a truncated file
  and an `InvalidFileHeader` that read exactly like a parser bug.

### 10.8 — Images: passthrough is the feature, and a blank logo is not a bug

All six image XObjects in the fixture now extract, and Pillow opens every one at the right
dimensions. Two rules did most of the work:

- **A `/DCTDecode` image is already a JPEG file.** Its bytes go straight to disk. Decoding
  and re-encoding would cost time and lose quality for nothing. `/JPXDecode` likewise.
- **Everything else is raw samples**, meaningless to a viewer, and gets packed into a PNG —
  lossless, so nothing degrades on the way.

`/Indexed` was worth supporting rather than refusing: two of the six images use it, so
without palette expansion the feature would have been "works on a third of this document".
Our expanded PNG is **pixel-identical** to PyMuPDF's extraction of the same object, which is
the check that made the next finding trustworthy.

**The finding that matters: a correct extraction can still be useless.** That 519x145
indexed image comes out entirely white — every pixel within one step of 255. It is not a
bug; PyMuPDF produces exactly the same pixels. The logo's whole shape lives in its
`/SMask`, and the base image is a blank rectangle. So `dropped_transparency` is not a note
about edge quality, it means *this picture may be meaningless on its own*. Compositing the
mask moved from "nice to have" to a real gap.

Two other honest gaps this exposed:
- **Images inside form XObjects are invisible to us.** The fixture contains DeviceGray
  images (974x272, 739x126) that never appear in the extraction, because they live in a
  form's own `/Resources` and we do not recurse into forms. The same recursion would fix
  the one form that contains text (§10.2 note).
- **Sub-byte sample depths are refused, not guessed.** 1-, 2- and 4-bit samples are packed
  several pixels to a byte against a row stride; unpacking them is real work, and none of it
  is guesswork, so it is deferred rather than faked.

### 10.7 — Measured against pdfium and PyMuPDF: what the competition gets wrong

Run `.venv/bin/python scripts/compare.py`. On the 44-page fixture:

```
                                   qalam      pdfium     pymupdf
  as returned
  logical word order                 yes          NO          NO
  lam-alef ligature                  yes          NO          NO
  tashkeel attached                  yes          NO          NO
  columns kept whole (of 3)            3           0           0
  presentation forms left              0           0       31080
  marks stranded on a space            0         138           1
  after the caller applies NFKC
  logical word order                 yes          NO         yes
  lam-alef ligature                  yes          NO         yes
  columns kept whole (of 3)            3           0           3
  presentation forms left              0           0           0
  marks stranded on a space            0         138           4
  speed (median of 5)              0.217s      0.313s      0.236s
```

**The second block is the honest one, and it changes the story.** PyMuPDF's word order,
ligatures and column handling are all *correct* — it simply returns presentation forms and
leaves normalisation to the caller. One `unicodedata.normalize("NFKC", …)` closes most of
the gap. Any comparison that omits this overstates our advantage, so the script reports both.

What survives that fairness test:

- **pdfium reverses word order**, and no downstream fix can recover it: `هذا الدليل إرشادي`
  comes back as `إلى الرجوع عن يغني … هذا`. Once glyphs are flattened into a string in the
  wrong sequence the information needed to undo it is gone. This is the one class of failure
  a caller genuinely cannot repair.
- **PyMuPDF already reconstructs columns**, which was a surprise — 3/3 cards intact on page 6.
  Geometric layout analysis is not our differentiator.
- **Tashkeel is the remaining difference, and it is NFKC's fault, not PyMuPDF's.** PyMuPDF
  places the marks correctly *after* their base; applying NFKC then inserts the placeholder
  space (§10.5) and yields `تتضم َّن`. Our `strip_mark_bases` is what closes it. So this is a
  bug any tool inherits by normalising naively — worth stating plainly rather than scoring as
  a win.
- **Neither tool tells you a page needs OCR.** Both return `""` for pages 2, 3, 42 and 43 —
  indistinguishable from a page that is genuinely blank. That, not raw accuracy, is the real
  differentiator, and it is the one thing the benchmark table cannot show.
- Speed is a non-issue: all three are within 1.4x on a 44-page document. Correctness was
  never going to be traded for it.

### 10.6 — The binding is thin because the orchestration moved down

Before M4 the CLI owned the pipeline sequence: build the font map, interpret, reconstruct,
assess — in that order, with L2 necessarily *before* L1 because the interpreter needs the
font map's advance widths. A second front end reimplementing that order is a second front
end that will drift out of step with it, silently.

So the sequence moved into `qalam_core::Document`, and both the CLI and the Python module
became presentation-only. `cli/src/extract.rs` fell from ~90 lines to ~40, with byte-identical
output. Rules this produced:

- **No pure-Python source tree.** The plan originally had a `python/qalam/__init__.py` for
  re-exports; with a pure-Rust module there is nothing to re-export, and PyO3 carries the
  docstrings. One fewer place for the two APIs to disagree.
- **Not every error is a custom exception.** A missing file raises `FileNotFoundError`, an
  unreadable one `PermissionError`, a bad page number `IndexError`. `QalamError` is reserved
  for what is genuinely qalam-specific: a malformed PDF. Matching on the wrapped
  `io::ErrorKind` is what makes the first two distinguishable.
- **Release the GIL around extraction.** Parsing touches no Python objects, so it runs under
  `py.detach`. Measured: a competing Python thread ran 3.6M iterations during a 0.22s
  extraction of the 44-page fixture. This is most of the reason a native extension is worth
  writing at all.
- **`Document.text` omits pages needing OCR, in Python too.** The honesty guarantee has to
  hold at the API boundary a caller actually touches, not just in the CLI's printing.

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
