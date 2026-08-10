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
        ("01-preface.html", "<html><head><style>body{margin:0 8px;font-family:serif;} p{margin:0 0 0.8em;}</style></head><body><h1>Preface</h1><p>A short opening that does not fill its column, so what follows it has somewhere to go.</p></body></html>".to_owned()),
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
