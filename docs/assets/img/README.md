# Article images

All four are in place and wired into `../../index.md`. Each is an
original-page-versus-qalam-output comparison rather than a diagram.

| file | shows | section |
|---|---|---|
| `hero-side-by-side.png` | A magazine spread and its `doc.to_html()` export, reading order intact. | opening |
| `columns-xy-cut.jpg` | The three-card page whose columns a global projection cannot find. | "The columns interleave" |
| `inspect-verdict.png` | Output where pages 2 and 3 read *"No text layer on this page — it needs OCR."* | "The failure that returns no error" |
| `table-to-csv.png` | A ruled governorate statistics table reconstructed as a grid. | "Beyond characters: tables" |

`logo.svg` is the README header mark, not an article image.

## If you replace one

- Keep the `<figure>` and `<figcaption>` wrapper. The captions carry argument, not labels.
- Export at ~1400 px wide (2× the ~700 px the theme renders at), then compress with `oxipng`
  or `pngquant`. `hero-side-by-side.png` is 328 KB and `table-to-csv.png` 168 KB; both are
  fine, but anything past ~400 KB is worth another pass.
- PNG for anything containing text. JPEG artefacts around Arabic diacritics read as rendering
  bugs, which in this particular article is an unfortunate thing to imply — `columns-xy-cut.jpg`
  is the one JPEG here and would be better as a PNG if you re-export it.
- Check the Arabic in the screenshot itself renders correctly. An article about tools that
  mangle Arabic loses badly if its own images show broken glyph shaping.
