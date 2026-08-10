//! Shaping and rasterizing with a real font.
//!
//! The renderer asks for glyphs by character and paints them back by id, so
//! the shaper owns the table that turns an id into something to draw. A
//! prepared frame hands out the shaper beside the painter for exactly this.
//!
//! Rasters are cached behind a `RefCell` because painting only ever borrows
//! the shaper: drawing a glyph the first time is a cache fill, not a change to
//! what the shaper says about any document.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use html::layout::{FontSlant, GlyphId, GlyphMetric, GlyphRegistry, GlyphShaper, ShapeError};

/// A rasterized glyph: coverage, and where to put it relative to the baseline
/// origin the renderer paints at.
pub struct GlyphRaster {
    pub coverage: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub left: i32,
    pub top: i32,
}

/// Weight and slant pick a face; everything else about a glyph is its
/// character and size.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    ch: char,
    size_bits: u32,
    bold: bool,
    italic: bool,
}

pub struct FontShaper {
    regular: fontdue::Font,
    bold: Option<fontdue::Font>,
    italic: Option<fontdue::Font>,
    /// What each id the renderer holds was shaped from.
    glyphs: Vec<GlyphKey>,
    /// Ids already handed out, so a repeated character keeps its id.
    ids: HashMap<GlyphKey, GlyphId>,
    rasters: RefCell<HashMap<GlyphKey, Rc<GlyphRaster>>>,
}

impl FontShaper {
    /// Loads the faces fontconfig answers with for a serif family.
    pub fn from_system_fonts() -> Result<Self, String> {
        Ok(Self {
            regular: load_face("serif")?,
            bold: load_face("serif:bold").ok(),
            italic: load_face("serif:italic").ok(),
            glyphs: Vec::new(),
            ids: HashMap::new(),
            rasters: RefCell::new(HashMap::new()),
        })
    }

    fn face(&self, key: GlyphKey) -> &fontdue::Font {
        match (key.bold, key.italic) {
            (true, _) => self.bold.as_ref().unwrap_or(&self.regular),
            (_, true) => self.italic.as_ref().unwrap_or(&self.regular),
            _ => &self.regular,
        }
    }

    /// The coverage bitmap for a glyph the renderer is painting, rasterized
    /// once and kept.
    pub fn raster(&self, glyph: GlyphId) -> Option<Rc<GlyphRaster>> {
        Some(self.rasterize(*self.glyphs.get(glyph as usize)?))
    }

    /// A character rasterized for the harness's own text -- status lines and
    /// labels, which are not part of any document.
    pub fn label_glyph(&self, ch: char, size: f32) -> (f32, Rc<GlyphRaster>) {
        let key = GlyphKey { ch, size_bits: size.to_bits(), bold: false, italic: false };
        (self.face(key).metrics(ch, size).advance_width, self.rasterize(key))
    }

    fn rasterize(&self, key: GlyphKey) -> Rc<GlyphRaster> {
        if let Some(raster) = self.rasters.borrow().get(&key) {
            return raster.clone();
        }
        let (metrics, coverage) = self.face(key).rasterize(key.ch, f32::from_bits(key.size_bits));
        let raster = Rc::new(GlyphRaster {
            coverage,
            width: metrics.width,
            height: metrics.height,
            left: metrics.xmin,
            // `ymin` is the bitmap's bottom edge above the baseline, so its top
            // edge is that far up plus the bitmap's own height.
            top: -(metrics.height as i32 + metrics.ymin),
        });
        self.rasters.borrow_mut().insert(key, raster.clone());
        raster
    }
}

fn load_face(pattern: &str) -> Result<fontdue::Font, String> {
    let output = std::process::Command::new("fc-match").arg("-f").arg("%{file}").arg(pattern).output().map_err(|error| format!("fc-match could not be run: {error}"))?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if path.is_empty() {
        return Err(format!("fontconfig matched no font for {pattern}"));
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("{path}: {error}"))?;
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).map_err(|error| format!("{path}: {error}"))
}

impl GlyphShaper for FontShaper {
    fn reset(&mut self) {
        // Ids index the document's registry, which starts over with the
        // document. Rasters are keyed by what was drawn, not by id, so they
        // survive.
        self.glyphs.clear();
        self.ids.clear();
    }

    fn shape_glyph<'a>(&mut self, glyph_metrics: &mut GlyphRegistry<'a>, ch: char, font_size: f32, font_weight: u16, font_slant: FontSlant, _color: u32, _family: Option<&str>) -> Result<GlyphId, ShapeError> {
        let key = GlyphKey { ch, size_bits: font_size.to_bits(), bold: font_weight >= 600, italic: !matches!(font_slant, FontSlant::Normal) };
        if let Some(&glyph) = self.ids.get(&key)
            && glyph_metrics.contains(glyph)
        {
            return Ok(glyph);
        }

        let face = self.face(key);
        let advance = face.metrics(ch, font_size).advance_width;
        let line = face.horizontal_line_metrics(font_size).unwrap_or(fontdue::LineMetrics { ascent: font_size * 0.8, descent: -font_size * 0.2, line_gap: 0.0, new_line_size: font_size });
        let metric = GlyphMetric::try_new(ch, advance.max(0.0), line.ascent.max(0.0), (-line.descent).max(0.0), 0.0).map_err(ShapeError::rejected_metric)?;

        let glyph = glyph_metrics.register(metric)?;
        if self.glyphs.len() <= glyph as usize {
            self.glyphs.resize(glyph as usize + 1, key);
        }
        self.glyphs[glyph as usize] = key;
        self.ids.insert(key, glyph);
        Ok(glyph)
    }
}
