#!/usr/bin/env python3
"""Convert a PDF to text, JSON or HTML with qalam.

Usage:
    python extract.py INPUT.pdf OUTPUT

The output's extension picks the format:
    report.txt    plain text, page by page, with each page's reliability
    report.json   structured: pages, verdicts, text blocks and tables
    report.html   a standalone web page, right-to-left, images included

If OUTPUT is a folder (or ends with "/"), all three files are written into it.

Prints a short summary. Exits with 1 if the PDF cannot be read.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

try:
    import qalam
except ImportError:
    sys.exit("qalam is not installed. Run: pip install qalam")

FORMATS = {".txt": "text", ".json": "json", ".html": "html", ".htm": "html"}


def to_text(doc) -> str:
    parts = []
    for page in doc:
        header = f"=== page {page.number} [{page.verdict}] ==="
        if page.reasons:
            header += f"  ({'; '.join(page.reasons)})"
        parts.append(header)
        # A needs_ocr page is not blank: its content is missing.
        parts.append("[no text layer — this page needs OCR]" if page.needs_ocr else page.text)
        parts.append("")
    return "\n".join(parts)


def to_json(doc) -> str:
    def block(b) -> dict:
        if b.kind == "table":
            # Columns are in reading order: on an Arabic page, column 0 is the rightmost.
            return {"kind": "table", "confidence": round(b.confidence, 3), "rows": b.to_rows()}
        if b.kind == "image":
            return {"kind": "image", "file_name": b.file_name, "width": b.width, "height": b.height}
        return {"kind": "text", "text": b.text}

    data = {
        "page_count": len(doc),
        "confidence": round(doc.confidence, 3),
        "pages_needing_ocr": doc.pages_needing_ocr,
        "pages": [
            {
                "number": page.number,
                "verdict": page.verdict,
                "confidence": round(page.confidence, 3),
                "reasons": list(page.reasons),
                "text": page.text,
                "blocks": [block(b) for b in page.blocks],
            }
            for page in doc
        ],
    }
    return json.dumps(data, ensure_ascii=False, indent=2)


def render(doc, fmt: str, title: str) -> str:
    if fmt == "json":
        return to_json(doc)
    if fmt == "html":
        return doc.to_html(title=title)
    return to_text(doc)


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] in ("-h", "--help"):
        print(__doc__.strip())
        return 0
    if len(sys.argv) != 3:
        print(__doc__.strip(), file=sys.stderr)
        return 1
    source, target = Path(sys.argv[1]), Path(sys.argv[2])

    as_folder = target.is_dir() or sys.argv[2].endswith(("/", "\\")) or target.suffix == ""
    if not as_folder and target.suffix.lower() not in FORMATS:
        print(f"error: unsupported output '{target.suffix}'; use .txt, .json, .html or a folder",
              file=sys.stderr)
        return 1

    try:
        doc = qalam.Document(str(source))
    except (FileNotFoundError, PermissionError, qalam.QalamError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    if as_folder:
        target.mkdir(parents=True, exist_ok=True)
        outputs = [(target / f"{source.stem}{ext}", fmt) for ext, fmt in
                   ((".txt", "text"), (".json", "json"), (".html", "html"))]
    else:
        target.parent.mkdir(parents=True, exist_ok=True)
        outputs = [(target, FORMATS[target.suffix.lower()])]

    for path, fmt in outputs:
        path.write_text(render(doc, fmt, source.stem), encoding="utf-8")

    degraded = [p.number for p in doc if p.verdict == "degraded"]
    print(f"{source.name}: {len(doc)} pages, confidence {doc.confidence:.2f}")
    print(f"needs OCR (content missing): {doc.pages_needing_ocr or 'none'}")
    print(f"degraded: {degraded or 'none'}")
    for path, _ in outputs:
        print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
