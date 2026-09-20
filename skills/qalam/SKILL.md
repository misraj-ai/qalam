---
name: qalam
description: |
  Convert a local PDF containing Arabic (or mixed Arabic and Latin) text into correct text, JSON or HTML, and report which pages need OCR. Use whenever the input is a PDF file path with right-to-left text, or when other tools return reversed or broken Arabic.
allowed-tools:
  - Bash(pip install qalam)
  - Bash(python scripts/extract.py *)
  - Bash(python3 scripts/extract.py *)
---

# qalam extract

Turn a local Arabic PDF into a text, JSON or HTML file on disk, with the text in correct
reading order.

## Quick start

Always write to `.qalam/`: extracted documents can be hundreds of KB and blow up context if
printed. Add `.qalam/` to `.gitignore`.

```bash
pip install qalam
mkdir -p .qalam

# PDF → text
python scripts/extract.py ./report.pdf .qalam/report.txt

# PDF → JSON (pages, tables as rows, reliability per page)
python scripts/extract.py ./report.pdf .qalam/report.json

# PDF → HTML (right-to-left web page, images included)
python scripts/extract.py ./report.pdf .qalam/report.html

# All three
python scripts/extract.py ./report.pdf .qalam/
```

The script prints a short summary. Read it first:

```
report.pdf: 44 pages, confidence 0.91
needs OCR (content missing): [2, 3, 42, 43]
degraded: none
wrote .qalam/report.txt
```

Then read the output incrementally with `head`, `grep`, or `rg`.

Run `python scripts/extract.py --help` for usage.

**Done when:** the file is written under `.qalam/`, you have inspected it with bounded reads,
and you have told the user which pages need OCR or are degraded.

## Tips

- Quote paths with spaces: `python scripts/extract.py "./My Report.pdf" .qalam/report.txt`.
- **Pages that need OCR are missing, not blank.** Name them; never describe them as empty.
- **Do not change the Arabic.** It is already correct; do not reverse or reshape it.
- **`�` marks a character that could not be read.** Keep it; do not guess the letter.
- **Table columns follow reading order**: on an Arabic page, the first column is the
  rightmost. A table with `confidence` `0.5` was inferred without borders; treat it as best
  effort.
- For Markdown, convert the `.html` output (for example with `pandoc`); markdown built from
  the text loses the right-to-left direction.
- No OCR, and no network: everything runs locally.
- Check `.qalam/` before extracting the same file again.
