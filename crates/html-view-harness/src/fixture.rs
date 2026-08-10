//! The spine the harness reads when it is not given one.
//!
//! Lengths are chosen so both continuations occur: chapters cut off by the
//! last column on screen, and short items that fit whole with room to spare.

use std::path::PathBuf;

fn chapter(heading: &str, title: &str, paragraphs: usize) -> String {
    let body = (0..paragraphs)
        .map(|index| {
            format!(
                "<p>Paragraph {} of {title}. The sentences here carry no meaning worth reading; they are here to occupy lines, and there are enough of them that the chapter outlasts the columns any one screen can give it.</p>",
                index + 1
            )
        })
        .collect::<String>();
    format!("<html><head><style>body{{margin:0 8px;font-family:serif;}} h1,h2{{margin:0 0 0.6em;}} p{{margin:0 0 0.8em;text-align:justify;}}</style></head><body><{heading}>{title}</{heading}>{body}</body></html>")
}

pub fn write_builtin_spine() -> Result<PathBuf, String> {
    let root = std::env::temp_dir().join("html-view-spine-harness");
    std::fs::create_dir_all(&root).map_err(|error| format!("{}: {error}", root.display()))?;

    // Numbered so the provider's sorted listing is the reading order.
    let documents = [
        ("01-preface.html", "<html><head><style>body{margin:0 8px;font-family:serif;} p{margin:0 0 0.8em;} a{font-size:1.25em;font-weight:bold;}</style></head><body><h1>Footnote popup test</h1><p>Click the numbered reference to open a popup at the click point: <a epub:type='noteref' role='doc-noteref' href='#popup-note'>[1]</a></p><p>Resize the window or move through the columns to exercise different viewport quadrants. Escape closes the popup.</p><aside id='popup-note' epub:type='footnote' role='doc-footnote'><p>This note is an overlay. It does not consume space in the document flow, and the popup corner touching the reference is chosen from the available viewport space.</p></aside></body></html>".to_owned()),
        ("02-chapter-one.html", chapter("h1", "Chapter One", 14)),
        ("03-interlude.html", "<html><head><style>body{margin:0 8px;font-family:serif;}</style></head><body><h2>Interlude</h2><p>Brief.</p></body></html>".to_owned()),
        ("04-chapter-two.html", chapter("h1", "Chapter Two", 11)),
        ("05-notes.html", chapter("h2", "Notes", 5)),
        ("06-colophon.html", "<html><head><style>body{margin:0 8px;font-family:serif;}</style></head><body><h2>Colophon</h2><p>Set in whatever fontconfig answers with for a serif.</p></body></html>".to_owned()),
    ];

    for (name, source) in documents {
        let path = root.join(name);
        std::fs::write(&path, source).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(root)
}
