use lopdf::Document;

/// M0: open a PDF and report its pages + the fonts on each page.
pub fn inspect(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let doc = Document::load(path)?;
    for (page_num, page_id) in doc.get_pages() {
        println!("Page {page_num} (object {page_id:?})");

        // Fonts declared in this page's /Resources.
        if let Ok(fonts) = doc.get_page_fonts(page_id) {
            for (name, dict) in fonts {
                let subtype = dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|o| o.as_name().ok())
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .unwrap_or_default();
                let has_tounicode = dict.get(b"ToUnicode").is_ok();
                println!(
                    "  font {} : subtype={} tounicode={}",
                    String::from_utf8_lossy(&name),
                    subtype,
                    has_tounicode, // <-- your earliest recoverability signal
                );
            }
        }
    }
    Ok(())
}
