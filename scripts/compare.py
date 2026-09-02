#!/usr/bin/env python3
"""Compare qalam against the two best free PDF text extractors.

Correctness first, speed second. Speed is easy and correctness is the whole
point: an extractor that is twice as fast and returns text you cannot search is
not a better tool.

Usage:
    .venv/bin/python scripts/compare.py [path/to/file.pdf]
    .venv/bin/python scripts/compare.py --dump comparison/ [path/to/file.pdf]

`--dump DIR` writes each extractor's raw output to a text file so the
differences can be read rather than summarised. It also writes an NFKC-folded
copy for the tools that return presentation forms, since that is the fair
basis for comparing them (see PLAN.md §10.7).

Requires `pypdfium2` and `pymupdf` alongside the built `qalam` module.
"""

from __future__ import annotations

import os
import statistics
import sys
import time
import unicodedata

FIXTURE = "tests/fixtures/test_for_arabic_barser.pdf"

# Arabic Presentation Forms-A and -B. A correct extractor emits none of these:
# they are *shaped glyphs*, not letters, and text containing them will not
# compare or search equal to the same words typed normally.
PRESENTATION_RANGES = ((0xFB50, 0xFDFF), (0xFE70, 0xFEFF))

# Page 4, verified by hand against the rendered document (PLAN.md §10.1).
PAGE4_PHRASE = "هذا الدليل إرشادي ولا يغني عن الرجوع إلى"
LIGATURE_OK = "إرشادي ولا يغني"
LIGATURE_BAD = "إرشادي وال يغني"

# Page 5: a word carrying a shadda, which splits in half if the mark's base is
# mishandled (PLAN.md §10.5).
TASHKEEL_WORD = "تتضمَّن"

# Page 6: three cards side by side. Each opening must stay contiguous, or the
# extractor is interleaving columns (PLAN.md §10.4).
CARD_SENTENCES = (
    "يخاطب هذا الدليل موظفي",
    "يهدف هذا الدليل إلى تطوير دور",
    "يتضمن الدليل الإرشادات وقائمة",
)


def count_presentation_forms(text: str) -> int:
    return sum(
        1 for c in text if any(lo <= ord(c) <= hi for lo, hi in PRESENTATION_RANGES)
    )


def count_stranded_marks(text: str) -> int:
    """Combining marks left sitting after a space, with no letter to attach to.

    The signature of NFKC's placeholder base leaking into the output.
    """
    return sum(
        1
        for a, b in zip(text, text[1:])
        if a == " " and unicodedata.category(b) == "Mn"
    )


# --- the extractors -------------------------------------------------------
#
# Each returns a dict of page number -> text, so the checks below are identical
# across tools and nothing is measured on a different basis.


def extract_qalam(path: str) -> dict[int, str]:
    import qalam

    doc = qalam.Document(path)
    return {p.number: p.text for p in doc}


def extract_pdfium(path: str) -> dict[int, str]:
    import pypdfium2

    doc = pypdfium2.PdfDocument(path)
    return {i + 1: doc[i].get_textpage().get_text_range() for i in range(len(doc))}


def extract_pymupdf(path: str) -> dict[int, str]:
    import pymupdf

    doc = pymupdf.open(path)
    return {i + 1: doc[i].get_text() for i in range(len(doc))}


EXTRACTORS = {
    "qalam": extract_qalam,
    "pdfium": extract_pdfium,
    "pymupdf": extract_pymupdf,
}


def time_it(fn, path: str, runs: int = 5) -> float:
    """Median wall-clock seconds over several runs.

    Median rather than mean so one unlucky run — a page fault, the scheduler —
    does not decide the number.
    """
    timings = []
    for _ in range(runs):
        start = time.perf_counter()
        fn(path)
        timings.append(time.perf_counter() - start)
    return statistics.median(timings)


def check(pages: dict[int, str], normalise: bool = False) -> dict[str, object]:
    """Score one extractor's output.

    With `normalise`, NFKC is applied first — the fair question of "how much of
    this could a caller fix downstream?". Word order cannot be fixed downstream;
    presentation forms usually can. Reporting both keeps the comparison honest
    about which failures are actually disqualifying.
    """
    if normalise:
        pages = {n: unicodedata.normalize("NFKC", t) for n, t in pages.items()}

    whole = "\n".join(pages.values())
    p4 = pages.get(4, "")
    p5 = pages.get(5, "")
    p6 = pages.get(6, "")

    return {
        "chars": len(whole),
        "presentation_forms": count_presentation_forms(whole),
        "stranded_marks": count_stranded_marks(whole),
        "word_order": PAGE4_PHRASE in p4,
        "ligature": LIGATURE_OK in p4 and LIGATURE_BAD not in p4,
        "tashkeel": TASHKEEL_WORD in p5,
        "columns": sum(1 for s in CARD_SENTENCES if s in p6),
    }


def dump(pages: dict[int, str], path: str) -> None:
    """Write one extractor's pages to a file, with a header per page.

    The page markers matter: the extractors disagree about how many characters
    a page holds, so without them the files cannot be lined up side by side.
    """
    with open(path, "w", encoding="utf-8") as fh:
        for number in sorted(pages):
            text = pages[number].rstrip()
            fh.write(f"{'=' * 70}\n== page {number}\n{'=' * 70}\n")
            fh.write(f"{text}\n" if text else "(no text returned)\n")
            fh.write("\n")


def main() -> int:
    argv = sys.argv[1:]

    dump_dir = None
    if argv and argv[0] == "--dump":
        if len(argv) < 2:
            print("--dump needs a directory", file=sys.stderr)
            return 1
        dump_dir, argv = argv[1], argv[2:]

    path = argv[0] if argv else FIXTURE

    results, folded, timings = {}, {}, {}
    for name, fn in EXTRACTORS.items():
        try:
            pages = fn(path)
            results[name] = check(pages)
            folded[name] = check(pages, normalise=True)
            timings[name] = time_it(fn, path)

            if dump_dir is not None:
                os.makedirs(dump_dir, exist_ok=True)
                dump(pages, os.path.join(dump_dir, f"{name}.txt"))
                # Only worth writing when normalisation changes something.
                if results[name]["presentation_forms"]:
                    folded_pages = {
                        n: unicodedata.normalize("NFKC", t) for n, t in pages.items()
                    }
                    dump(folded_pages, os.path.join(dump_dir, f"{name}-nfkc.txt"))
        except ImportError as exc:
            print(f"skipping {name}: {exc}", file=sys.stderr)

    if not results:
        print("no extractors available", file=sys.stderr)
        return 1

    names = list(results)
    width = max(len(n) for n in names) + 2

    def row(label: str, values, fmt=str) -> None:
        cells = "".join(f"{fmt(v):>16}" for v in values)
        print(f"  {label:<26}{cells}")

    tick = lambda b: "yes" if b else "NO"  # noqa: E731

    print(f"\n{path}\n")
    print(f"  {'':<26}" + "".join(f"{n:>16}" for n in names))
    print("  " + "-" * (26 + 16 * len(names)))
    print("  as returned")
    row("logical word order", [results[n]["word_order"] for n in names], tick)
    row("lam-alef ligature", [results[n]["ligature"] for n in names], tick)
    row("tashkeel attached", [results[n]["tashkeel"] for n in names], tick)
    row("columns kept whole (of 3)", [results[n]["columns"] for n in names])
    row("presentation forms left", [results[n]["presentation_forms"] for n in names])
    row("marks stranded on a space", [results[n]["stranded_marks"] for n in names])
    print("  after the caller applies NFKC")
    row("logical word order", [folded[n]["word_order"] for n in names], tick)
    row("lam-alef ligature", [folded[n]["ligature"] for n in names], tick)
    row("tashkeel attached", [folded[n]["tashkeel"] for n in names], tick)
    row("columns kept whole (of 3)", [folded[n]["columns"] for n in names])
    row("presentation forms left", [folded[n]["presentation_forms"] for n in names])
    row("marks stranded on a space", [folded[n]["stranded_marks"] for n in names])
    print("  output")
    row("characters", [f"{results[n]['chars']:,}" for n in names])
    print("  speed")
    row("median of 5 runs", [f"{timings[n]:.3f}s" for n in names])

    fastest = min(timings.values())
    row("relative", [f"{timings[n] / fastest:.1f}x" for n in names])

    if dump_dir is not None:
        print(f"\n  wrote raw output to {dump_dir}/")

    print(
        "\n  The second block is the fair question: how much of this could a"
        "\n  caller repair themselves? Presentation forms usually can — NFKC"
        "\n  folds them, and it expands ligatures correctly *provided the text"
        "\n  was already reordered* (PLAN.md §3). Word order and reading order"
        "\n  cannot: once the glyphs have been flattened into a string in the"
        "\n  wrong sequence, the information needed to undo it is gone.\n"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
