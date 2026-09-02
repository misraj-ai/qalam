# qalam

Correct, logical-order **Arabic text extraction** from digitally-born PDFs — without OCR.

Most PDF extractors hand back Arabic that is reversed, made of presentation forms, or
silently corrupted at every ligature. `qalam` fixes the reading order, folds the shaped
glyphs back to base letters, reconstructs multi-column reading order, and — when a page
has no usable text layer — **says so** instead of returning plausible-looking garbage.

```python
import qalam

text = qalam.extract_text("guide.pdf")

doc = qalam.Document("guide.pdf")
print(doc.confidence)          # 0.91
print(doc.pages_needing_ocr)   # [2, 3, 42, 43]

for page in doc:
    if page.needs_ocr:
        print(page.number, page.reasons)
    else:
        print(page.text)
```

`Document.text` omits pages that need OCR, so a scanned page never masquerades as a
result. Read `page.text` directly if you want to see it anyway.

Each line carries its geometry and styling — `line.direction`, `line.size`, `line.color`,
`line.confidence` — enough to rebuild a styled document.

Built on a Rust core; the GIL is released during extraction.
