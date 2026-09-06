<div align="center">

[//]: # (<img src="docs/assets/img/logo.svg" alt="qalam" width="120">)

# qalam &nbsp;·&nbsp; <span dir="rtl">قلم</span>

**Correct, logical-order Arabic text extraction from digitally-born PDFs — without OCR.**

<!-- Badges. Each is a live link, not decoration: click-through matters more than colour. -->

[![PyPI](https://img.shields.io/pypi/v/qalam?logo=pypi&logoColor=white&label=PyPI&color=3775A9)](https://pypi.org/project/qalam/)
[![Rust](https://img.shields.io/badge/Rust-stable-000000?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Release](https://img.shields.io/github/actions/workflow/status/misraj-ai/qalam/release.yml?logo=githubactions&logoColor=white&label=release)](https://github.com/misraj-ai/qalam/actions/workflows/release.yml)


### [📖 Read the article](https://misraj-ai.github.io/qalam/) &nbsp;·&nbsp; [🚀 Quick start](#quick-start) &nbsp;·&nbsp; [📚 API](#using-the-python-api) &nbsp;·&nbsp; [🤝 Contributing](CONTRIBUTING.md) &nbsp;·&nbsp; [💬 Discussions](https://github.com/misraj-ai/qalam/discussions)

</div>

---

Most PDF extractors return Arabic that is reversed, made of presentation forms, or silently
corrupted at every ligature. `qalam` fixes the reading order, folds shaped glyphs back to base
letters, reconstructs multi-column reading order and ruled tables, and when a page has no
usable text layer **says so** rather than returning plausible-looking garbage.

<table>
<tr>
<th align="center" width="50%">Other extractors</th>
<th align="center" width="50%">qalam</th>
</tr>
<tr>
<td align="center"><p dir="rtl" lang="ar">إلى الرجوع عن يغني ولا إرشادي الدليل هذا</p><sub>every word reversed — unrecoverable downstream</sub></td>
<td align="center"><p dir="rtl" lang="ar">هذا الدليل إرشادي ولا يغني عن الرجوع إلى</p><sub>logical order, base letters, ligature intact</sub></td>
</tr>
<tr>
<td align="center"><p dir="rtl" lang="ar">ﺍﻟﺪﻟﻴﻞ ﺍﻹﺭﺷﺎﺩﻱ</p><sub>presentation forms — shares no codepoint with the real text</sub></td>
<td align="center"><p dir="rtl" lang="ar">الدليل الإرشادي</p><sub>base letters</sub></td>
</tr>
<tr>
<td align="center"><p dir="rtl" lang="ar">تتضم َّن</p><sub>a space in the middle of the word</sub></td>
<td align="center"><p dir="rtl" lang="ar">تتضمَّن</p><sub>marks attached to their base</sub></td>
</tr>
<tr>
<td align="center"><code>""</code><sub><br>a scanned page and a blank page, indistinguishable</sub></td>
<td align="center"><code>page.verdict == "needs_ocr"</code><sub><br>says what it could not read</sub></td>
</tr>
</table>

> **Why these examples matter:** every string above is real output from a real government PDF,
> reproducible with `python scripts/compare.py`. The story behind each one is in
> **[the article](https://misraj-ai.github.io/qalam/)**.

---

## Quick start

```sh
pip install qalam
```

```python
import qalam

text = qalam.extract_text("guide.pdf")

doc = qalam.Document("guide.pdf")
print(doc.confidence)          # 0.91
print(doc.pages_needing_ocr)   # [2, 3, 42, 43]
```

---

## Why this exists

A PDF does not store text. It stores instructions to *paint glyphs at positions*, and
extraction is the inverse problem. Arabic stresses every weak point in it:

| Failure | What you get |
|---|---|
| Glyphs are stored as **presentation forms** | `ﻣﺤﻤﺪ` instead of `محمد` |
| Glyphs are painted in **visual order** | every word reversed |
| A ligature is **one glyph, two letters** | `ولا` becomes `وال` |
| The `/ToUnicode` map is **missing or wrong** | silent corruption, or nothing at all |
| Text sits in **columns** | three paragraphs interleaved line by line |

The ordering rule that most tools get wrong: **reorder before normalising.** NFKC expands the
lam-alef ligature `ﻻ` into two characters; reorder afterwards and they swap. `qalam` reorders
while ligatures are still single glyphs, then normalises.

---

## Installing

### Python

## using pip 

```sh
pip install qalam==0.1.0
```
if you have some problem build from the project

## Build from project
Requires **Python ≥ 3.9** and a **Rust toolchain** (stable) to build from source. Wheels are
`abi3`, so one build serves every Python from 3.9 up.
```sh
git clone <repo-url> && cd qalam
python -m venv .venv && . .venv/bin/activate
pip install maturin
maturin develop --release --manifest-path crates/qalam-py/Cargo.toml
```

Then `import qalam` works in that environment.

To build a redistributable wheel instead:

```sh
maturin build --release --manifest-path crates/qalam-py/Cargo.toml
pip install target/wheels/qalam-*.whl
```

### Rust

```toml
[dependencies]
qalam-core = { path = "crates/qalam-core" }
```

### Command line

```sh
cargo build --release
./target/release/qalam extract file.pdf
```

---

## Using the Python API

### The simple case

```python
import qalam

text = qalam.extract_text("guide.pdf")
```

`extract_text` returns the whole document. **Pages with no usable text layer contribute
nothing** — a scanned page never masquerades as a result.

### Documents and pages

```python
doc = qalam.Document("guide.pdf")

len(doc)                  # page count
doc.text                  # whole document, unreadable pages omitted
doc.confidence            # mean per-page confidence, 0.0–1.0
doc.pages_needing_ocr     # [2, 3, 42, 43] — hand these to an OCR engine

for page in doc:          # a Document iterates its pages
    page.number           # 1-based
    page.width, page.height, page.rotation
    page.text             # this page, in reading order
    page.verdict          # "ok" | "degraded" | "needs_ocr"
    page.confidence
    page.needs_ocr
    page.reasons          # why, when it is not "ok"
    page.tagged           # order came from the PDF's structure tree, not geometry
```

`doc.page(4)` looks a page up by number and raises `IndexError` if it does not exist.

### Blocks: structure, not just a string

A page is an ordered list of typed blocks. Each has a `kind`, so you can branch without
`isinstance`:

```python
for block in doc.page(6).blocks:
    if block.kind == "table":
        for row in block.to_rows():
            print(row)
    elif block.kind == "image":
        block.save(block.file_name)
    else:
        print(block.text)
```

Every kind answers `.text`, so the flat and structured views agree exactly:

```python
page.text == "\n".join(b.text for b in page.blocks if b.text)   # True
```

`page.tables`, `page.images` and `page.lines` are filtered views over the same objects — no
second extraction, nothing to drift out of step.

### Tables

Columns are numbered in **reading order**, so on an Arabic page `rows[0][0]` is the *rightmost*
cell — where a reader starts.

```python
t = doc.page(5).tables[0]
t.row_count, t.column_count
t.confidence                   # how much of the grid was actually drawn
t.rows[1][0].text              # 'المحافظات'
t.to_rows()                    # list[list[str]] — the shape csv.writer wants

import csv
with open("out.csv", "w", newline="") as f:
    csv.writer(f).writerows(t.to_rows())
```

Each `Cell` has `.text`, `.row`, `.column` and `.bbox`.

### Images

```python
for img in doc.page(1).images:
    if img.unsupported_reason:
        print("not decoded:", img.unsupported_reason)
        continue
    img.save(img.file_name)     # "Im0.jpg"
    img.width, img.height, img.format
    img.data                    # bytes
    img.is_background           # covers most of the page
    img.dropped_transparency    # see the caveat below
```

JPEG images pass through untouched — a `/DCTDecode` image already *is* a JPEG file, and
re-encoding would lose quality for nothing. Everything else is re-encoded losslessly as PNG.

> **`dropped_transparency` matters more than it sounds.** A logo is routinely stored as a
> *blank* image whose entire shape lives in its `/SMask`. Compositing the mask is not yet
> implemented, so such an image extracts byte-correctly and looks empty.

### Lines and styling

```python
for line in doc.page(4).lines:
    line.text
    line.direction      # "rtl" | "ltr"
    line.font, line.size, line.color   # e.g. "C2_0", 20.6, "#0a406b"
    line.baseline
    line.confidence
```

Effective size is composed from the text matrix, not read off `Tf` — writers routinely emit
`/C2_0 1 Tf` and put the real scale in the matrix.

### HTML output

```python
html = doc.to_html()                                   # standalone page
html = doc.to_html(include_images=False, title="…")    # smaller, no data: URIs

body_size, headings = doc.heading_sizes()
# 14.0, [(26.0, 1), (24.0, 2), (18.0, 3)]
```

Reading order, headings, colour, tables with `dir="rtl"` and images inlined as `data:` URIs —
one self-contained file, no assets. Use `include_images=False` for a document full of
pictures; on the test corpus that is 308 KB versus 73 KB.

**Headings are inferred, not read.** A PDF never says "this is a heading" — it says some text
is larger. `qalam` finds the body size by character weight and maps larger sizes to `h1`–`h6`.
`heading_sizes()` reports what was assumed, because it is a guess. A document that signals
headings by weight or colour rather than size comes back with none: flat, not wrong.

Two things are deliberately dropped. **Near-white text loses its colour** — in the PDF it sits
on a coloured banner, and since page backgrounds are not reproduced, honouring it would paint
white text on a white page. And **lines within a block are joined with a space**, not `<br>`,
because block boundaries already came from the layout pass and the line breaks inside one are
the column width talking, not the author.

For Markdown, run the HTML through `turndown`, `pandoc` or `html2text`. Markdown cannot state
text direction, so an Arabic table — whose first column is the *rightmost* — comes out
mirrored with no way to fix it. HTML → Markdown is easy; Markdown → correct RTL is not.

### Errors

```python
qalam.QalamError      # a malformed PDF
FileNotFoundError     # no such file
PermissionError       # cannot read it
IndexError            # no such page
```

Only what is genuinely qalam-specific gets a custom exception; the rest map to the exceptions
Python programmers already catch.

### Threading

The GIL is released during extraction, so other Python threads keep running. Measured: a
competing thread ran 3.6M iterations during a 0.22 s extraction of a 44-page document.

---

## Using the Rust API

```rust
use qalam_core::{Document, Block};

let doc = Document::open("guide.pdf")?;
println!("{}", doc.text());

for page in doc.pages() {
    if page.needs_ocr() {
        eprintln!("page {} needs OCR: {:?}", page.number, page.report.reasons);
        continue;
    }
    for block in &page.blocks {
        match block {
            Block::Text(t) => println!("{}", t.text()),
            Block::Table(t) => println!("{}x{} table", t.table.row_count(), t.table.column_count()),
            Block::Image(_) => {}
        }
    }
}
# Ok::<(), qalam_core::Error>(())
```

Extraction is eager: by the time you hold a `Document`, every page has been read, resolved and
judged, so no accessor can fail.

---

## Command line

```sh
qalam extract <file.pdf> [page|all]   # correct logical Arabic
qalam blocks  <file.pdf> [page]       # typed blocks in reading order
qalam html    <file.pdf> [out.html]  # render to a standalone HTML page
qalam images  <file.pdf> [dir]        # list, or write images to a directory
qalam inspect <file.pdf>              # pages, geometry, fonts, recoverability
```

Three lower-level commands exist for debugging the pipeline: `raw` (the content stream),
`glyphs` (positioned glyph codes, L1) and `text` (codes resolved through `/ToUnicode`, L2).

---

## The recoverability verdict

The feature that distinguishes `qalam`. Every page is judged:

| Verdict | Meaning |
|---|---|
| `ok` | Every glyph resolved; the text can be trusted. |
| `degraded` | Text came out, but something is wrong — read `page.reasons`. |
| `needs_ocr` | No usable text layer. Emitting the text would be worse than useless. |

`ok` requires that **everything** resolved: any unresolvable glyph is a character missing from
the output, and a tolerance there would let real losses pass as clean.

A page with no glyphs scores **0.0**, not 1.0 — "nothing failed" is not "everything worked",
and that is exactly where a naive metric reports a perfect score for a scan.

---

## How it works

The pipeline is layered so each stage is independently testable, and modules map onto it 1:1.

```
  L0  parser      PDF object graph → pages, fonts, images, forms
  L1  content     content stream → positioned, styled glyph codes
      graphics    transforms and colour
  L1.5 structure  tagged /StructTreeRoot → stated reading order
  L2  font        glyph code → Unicode
      encoding    /Encoding + /Differences
      cff         embedded font program's glyph names
  L3  bidi        visual order → logical order (UAX #9)
      arabic      line grouping, then reorder, then NFKC
  L4  detect      recoverability scoring
  L6  layout      recursive XY-cut: columns, RTL reading order
  L7  images      /XObject extraction
      tables      ruled-line grids
      blocks      the unified ordered model
```

`PLAN.md` carries the design in full, plus a **findings log** (§10) recording what each real
document taught us — including several bugs that produced correct-looking, wrong output.

---

## What it does not do

- **OCR.** Scanned pages are *detected* and reported, not read.
- **PDF creation or editing.**
- **Pixel-perfect layout.** Ordered typed blocks, not a visual clone.
- **Borderless tables.** Ruled tables only; alignment-inferred tables are a stretch goal.
- **Page backgrounds in HTML output.** Images are placed in the flow, not behind the text.
- **`/SMask` compositing**, `/FontFile` (Type 1) and `/FontFile2` (TrueType) glyph names,
  and text rotated to angles other than quarter turns.

`PLAN.md` §8 keeps the full list of known limits and open questions.

---

## Status

This is young software validated against a small corpus. **More Arabic PDFs is the single
most valuable contribution** — especially ones that break something. Every rule in the
pipeline exists because a real document proved the previous rule wrong.

---

## Community

<table>
<tr>
<td width="33%" valign="top">

### 🐛 Found a bad extraction?

[**Open an issue**](https://github.com/misraj-ai/qalam/issues/new) — and attach the PDF if you
can share it. A file that breaks something is worth more than a bug report about it.

</td>
<td width="33%" valign="top">

### 💬 Have a question?

[**Start a discussion**](https://github.com/misraj-ai/qalam/discussions). Questions about
whether qalam fits your pipeline are welcome; the answer is sometimes no.

</td>
<td width="33%" valign="top">

### 🤝 Want to contribute?

Read [**CONTRIBUTING.md**](CONTRIBUTING.md). `PLAN.md` §10 carries the findings log — what each
real document taught us, including the bugs that produced correct-looking, wrong output.

</td>
</tr>
</table>

**The highest-value contribution is a PDF.** Arabic documents that extract badly are the
bottleneck on this project — every correctness rule described in the article came from one.
Government publications, financial reports, theses, anything with tables. If you cannot share
the file, the output of `qalam inspect yourfile.pdf` is still useful.

---

## Learn more

| | |
|---|---|
| 📖 **[The article](https://misraj-ai.github.io/qalam/)** | Why every Arabic PDF extractor is quietly wrong, and what it takes to fix it. Start here. |
| 📐 **[`PLAN.md`](PLAN.md)** | The full design, plus a findings log recording what each real document taught us. |
| 🤝 **[`CONTRIBUTING.md`](CONTRIBUTING.md)** | How to build, test, and what kind of help is most useful. |
| 📄 **[`paper/`](paper/)** | An academic write-up of the same material, for citation. |

---

## Licence

MIT — see [`LICENSE`](LICENSE). Copyright © 2026 Misraj AI.

<div align="center">
<br>
<sub>Built with 🦀 Rust and a great deal of patience for the PDF specification.</sub>
</div>
