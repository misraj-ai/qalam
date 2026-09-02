# Contributing to qalam

Thank you for looking. This document covers getting set up, the conventions the codebase
follows, and — most importantly — the one principle everything else follows from.

---

## The principle

**Never invent text.**

Every other rule here is a consequence. When a glyph cannot be resolved, we emit `U+FFFD`, not
nothing — because "we cannot read this" and "there was nothing here" are different facts, and
collapsing them is the exact deception this project exists to prevent. When a page has no
usable text layer, we say `needs_ocr` rather than returning a plausible fragment. When an
image uses a colour space we do not model, it comes back with a *reason*, not silently
dropped.

The failure mode we care most about is **output that looks right and is wrong**. A garbled
page gets noticed; a number with a misplaced decimal point does not. Several bugs in the
findings log were of exactly that kind, and they are why the tests are as fussy as they are.

If you find yourself writing "this is probably a comma", stop and ask what evidence you have.
If the answer is "it usually is", the honest move is to report the ambiguity rather than
resolve it. See `PLAN.md` §10.15 for a worked example of when guessing *is* justified — and
what made it so.

---

## Setting up

You need a **stable Rust toolchain** and **Python ≥ 3.9**.

```sh
git clone <repo-url> && cd qalam

# Rust side
cargo build
cargo test

# Python side
python -m venv .venv && . .venv/bin/activate
pip install maturin
maturin develop --release --manifest-path crates/qalam-py/Cargo.toml
python -c "import qalam; print(qalam.__doc__)"
```

For the benchmark script you also need the tools it compares against:

```sh
pip install pypdfium2 pymupdf pillow
python scripts/compare.py
```

---

## Running the tests

```sh
cargo test                    # everything: unit, golden, tagged, doc tests
cargo clippy --all-targets    # must be clean
cargo fmt                     # must be clean
```

There are four kinds of test, and they answer different questions.

**Unit tests**, beside the code in `src/`. They check one piece in isolation.

**Golden-file tests** (`crates/qalam-core/tests/golden.rs`) run the whole pipeline over the
corpus and diff the result against `tests/expected/`. They are the safety net: any change to
any layer shows up, so a "harmless" tweak to bidi cannot silently alter page 30.

After an *intended* change, regenerate and **read the diff before committing it**:

```sh
UPDATE_GOLDEN=1 cargo test -p qalam-core --test golden
git diff tests/expected
```

A golden diff says *something* changed. The named regressions beside it say *what* broke —
`ligatures_survive_the_reorder`, `columns_are_not_interleaved`,
`no_table_is_invented_in_the_untagged_corpus`, and so on. Each is a bug that actually
happened. Add one whenever you fix a bug that the golden file alone would only report as
"line 47 changed".

**Synthetic-PDF tests** (`crates/qalam-core/tests/tagged.rs`) build real PDFs — real objects,
a real xref table — so features can be exercised on inputs the corpus lacks. Note the limit
honestly: a fixture you write yourself can only test behaviour you already thought of. That
is not hypothetical; see `PLAN.md` §10.12.

**The comparison script** (`scripts/compare.py`) measures correctness and speed against
pdfium and PyMuPDF. It reports results both as-returned *and* after the caller applies NFKC,
because omitting the second block would overstate our advantage.

---

## The most valuable contribution: more PDFs

The corpus is two documents. Every threshold in `detect.rs`, `layout.rs` and `tables.rs` is a
judgement call until measured against more.

The second fixture immediately exposed a whole class of ligature bug the first could not
reach, and proved that a tagged-PDF reader which passed all its synthetic tests produced
worse output than geometry on real input. **Both were found by adding one file.**

Particularly wanted:

- Arabic PDFs from producers other than InDesign and Adobe Distiller
- **Tagged** PDFs (`/StructTreeRoot`) from Word or an accessibility workflow
- Documents with `/ActualText` — implemented, but exercised only against synthetic streams
- Borderless tables, RTL forms, mixed Arabic/Latin/numeric lines
- Anything where our output is worse than PyMuPDF's

To add one:

1. Put the file in `tests/fixtures/`.
2. Add it to the `CORPUS` array in `crates/qalam-core/tests/golden.rs`.
3. `UPDATE_GOLDEN=1 cargo test -p qalam-core --test golden`
4. **Read the generated golden file.** Compare against the rendered page — `PLAN.md` §10 has
   several examples of doing this with PyMuPDF's rasteriser. If anything is wrong, that is a
   bug worth a named test, not a golden file to accept.

Please check you have the right to redistribute a document before adding it.

---

## Project layout

```
crates/qalam-core/     the library; knows nothing about Python or the CLI
crates/qalam-py/       PyO3 bindings, deliberately thin
cli/                   the `qalam` command
scripts/compare.py     correctness and speed vs pdfium / PyMuPDF
tests/fixtures/        the corpus
tests/expected/        golden output, one file per fixture
PLAN.md                design, milestones, open questions, findings log
```

`qalam-py` is excluded from the workspace's `default-members`, because a Python extension
module's test harness would need to link libpython and break `cargo test` at the repo root.
maturin builds it explicitly.

---

## Layering

Modules map onto the pipeline (see `README.md`), and the dependencies run one way. Two rules
in particular:

- **Only `parser.rs` knows `lopdf` exists.** It turns the object graph into the plain types in
  `types.rs`. If lopdf ever has to go, one file changes — and, just as usefully, the CMap
  parser can be tested on a byte string with no PDF in sight.
- **Layers take plain data, not the layer above's types.** `arabic.rs` accepts image
  placements as bare `Rect`s: reading order is a property of *where things are*, so L3 needs
  nothing about what they contain and stays free of any dependency on L7.

The one rule that is easy to break and expensive to debug: **reorder before normalising**.
NFKC expands the lam-alef ligature into two characters; a reorder afterwards swaps them.
`arabic.rs` has a test that performs the steps in the wrong order on purpose and asserts the
damage, so anyone tempted to swap them sees exactly what breaks.

---

## Code style

This codebase is commented far more heavily than most, on purpose. It is also a teaching
artifact: comments explain *why*, and Rust idioms are explained where they first appear.

- **Comment the decision, not the mechanism.** `// increment i` is noise. `// `re W n` paints
  nothing — treating path construction as painting would draw a border round every page` is
  the reason a future reader needs.
- **Record the evidence.** When a constant comes from a real document, say which and what it
  measured. Several thresholds cite the page that produced them.
- **Name what a failure means.** `ExtractedImage::Unsupported { reason }` rather than
  `Option::None`, so a caller can say *why*.
- **Bounds-check everything read from a file.** A font program is arbitrary bytes we did not
  write; a panic in a library is a denial of service for whoever embedded it.
- **Never compare floats with `==`**, and use `total_cmp` where a sort needs a total order —
  a NaN coordinate from a malformed file must not corrupt a sort or panic.

`cargo fmt` and `cargo clippy --all-targets` must both be clean. `#![warn(missing_docs)]` is
on for the library, so public items need doc comments.

---

## The findings log

`PLAN.md` §10 records what real documents taught us. It is the most useful part of the
repository for anyone new, because most entries describe a bug that produced *correct-looking,
wrong* output.

If you fix a bug that a reasonable person would have shipped, add an entry. State what the
document did, what we did wrong, and what rule came out of it. When a later finding
contradicts an earlier one, **mark the old entry superseded rather than editing it** — a log
that silently rewrites itself teaches nothing.

---

## Submitting a change

Before opening a pull request:

1. `cargo test` passes.
2. `cargo clippy --all-targets` and `cargo fmt --check` are clean.
3. If output changed, the golden files are regenerated **and you have read the diff**.
4. A bug fix has a named regression test.
5. If it changes what a real document produces, say so in the description and show the before
   and after.

Small, focused changes are much easier to review than large ones. If a change turns out to
need work across several layers, say so — that is usually a sign the layering needs a look.
