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

## Structure, not just a string

A page is an ordered list of typed blocks. Each carries a `kind`, so you can branch on it
without reaching for `isinstance`.

```python
for block in doc.page(6).blocks:
    if block.kind == "table":
        for row in block.to_rows():      # the shape csv.writer wants
            print(row)
    elif block.kind == "image":
        block.save(block.file_name)
    else:
        print(block.text)
```

`page.tables`, `page.images` and `page.lines` are filtered views over the same blocks.

Tables know their reading order: on an Arabic page `rows[0][0]` is the **rightmost** cell,
where a reader starts.

```python
t = doc.page(5).tables[0]
t.row_count, t.column_count, t.confidence
t.rows[1][0].text          # 'المحافظات'
```

Images come back decoded — JPEG passed through untouched, everything else re-encoded as
PNG — or with an `unsupported_reason` saying why not. Never silently dropped.

Each line carries its geometry and styling — `line.direction`, `line.size`, `line.color`,
`line.confidence` — enough to rebuild a styled document.

`page.tagged` says whether the reading order came from the document's own structure tree
rather than from geometry.

Built on a Rust core; the GIL is released during extraction.
