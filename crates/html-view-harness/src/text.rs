//! Shaping and rasterizing with a real font.
//!
//! The renderer asks for glyphs by character and paints them back by id, so
//! the shaper owns the table that turns an id into something to draw. A
//! prepared frame hands out the shaper beside the painter for exactly this.
//!
//! The painter owns a mutable raster cache separately from the shared shaper.
//! Filling it does not change the document's glyph registry.
//!
//! One registry owns handles across the book, including popup notes from
//! other spine items. Shaping and painting need no host-selected document.

use html::text::{GlyphRequest, FontMetrics, ShapedGlyph};
use std::collections::HashMap;
use std::rc::Rc;

use html::text::{FontSlant, GlyphHandle, GlyphResourceGeneration, GlyphShaper, ShapeError};

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

/// Glyph handles retained across the book.
#[derive(Default)]
struct GlyphRegistry {
    /// What each id the renderer holds was shaped from.
    keys: Vec<GlyphKey>,
    /// Ids already handed out, so a repeated character keeps its id.
    ids: HashMap<GlyphKey, GlyphHandle>,
    generation: GlyphResourceGeneration,
}

/// Paint-time rasters shared by page, note and interface text across frames.
#[derive(Default)]
pub struct RasterCache(HashMap<GlyphKey, Rc<GlyphRaster>>);

pub struct FontShaper {
    regular: fontdue::Font,
    bold: Option<fontdue::Font>,
    italic: Option<fontdue::Font>,
    glyphs: GlyphRegistry,
    document_checkpoint: Option<usize>,
    append_checkpoint: Option<usize>,
}

impl FontShaper {
    /// Loads the faces fontconfig answers with for a serif family.
    pub fn from_system_fonts() -> Result<Self, String> {
        Ok(Self {
            regular: load_face("serif")?,
            bold: load_face("serif:bold").ok(),
            italic: load_face("serif:italic").ok(),
            glyphs: GlyphRegistry::default(),
            document_checkpoint: None,
            append_checkpoint: None,
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
    /// once and kept. Handles share one registry across pages and notes.
    pub fn raster(&self, glyph: GlyphHandle, rasters: &mut RasterCache) -> Option<Rc<GlyphRaster>> {
        let key = *self.glyphs.keys.get(glyph.0 as usize)?;
        Some(self.rasterize(key, rasters))
    }

    /// A character rasterized for the harness's own text -- status lines and
    /// labels, which are not part of any document.
    pub fn label_glyph(
        &self,
        ch: char,
        size: f32,
        rasters: &mut RasterCache,
    ) -> (f32, Rc<GlyphRaster>) {
        let key = GlyphKey {
            ch,
            size_bits: size.to_bits(),
            bold: false,
            italic: false,
        };
        (
            self.face(key).metrics(ch, size).advance_width,
            self.rasterize(key, rasters),
        )
    }

    fn rasterize(&self, key: GlyphKey, rasters: &mut RasterCache) -> Rc<GlyphRaster> {
        if let Some(raster) = rasters.0.get(&key) {
            return raster.clone();
        }
        let (metrics, coverage) = self
            .face(key)
            .rasterize(key.ch, f32::from_bits(key.size_bits));
        let raster = Rc::new(GlyphRaster {
            coverage,
            width: metrics.width,
            height: metrics.height,
            left: metrics.xmin,
            // `ymin` is the bitmap's bottom edge above the baseline, so its top
            // edge is that far up plus the bitmap's own height.
            top: -(metrics.height as i32 + metrics.ymin),
        });
        rasters.0.insert(key, raster.clone());
        raster
    }
}

fn load_face(pattern: &str) -> Result<fontdue::Font, String> {
    let output = std::process::Command::new("fc-match")
        .arg("-f")
        .arg("%{file}")
        .arg(pattern)
        .output()
        .map_err(|error| format!("fc-match could not be run: {error}"))?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if path.is_empty() {
        return Err(format!("fontconfig matched no font for {pattern}"));
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("{path}: {error}"))?;
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
        .map_err(|error| format!("{path}: {error}"))
}

impl GlyphShaper for FontShaper {
    fn reset(&mut self) {
        self.glyphs = GlyphRegistry::default();
    }

    fn resource_generation(&self) -> GlyphResourceGeneration {
        self.glyphs.generation
    }

    fn begin_document_shaping(&mut self) {
        assert!(self.document_checkpoint.is_none());
        // Handles are book-wide. Keep earlier pages and notes paintable while
        // retaining only a length checkpoint for resources appended here.
        self.document_checkpoint = Some(self.glyphs.keys.len());
    }

    fn commit_document_shaping(&mut self) {
        self.document_checkpoint = None;
    }

    fn rollback_document_shaping(&mut self) {
        if let Some(len) = self.document_checkpoint.take() {
            self.glyphs.keys.truncate(len);
            self.glyphs.ids.retain(|_, glyph| (glyph.0 as usize) < len);
        }
    }

    fn begin_append_shaping(&mut self) -> Result<(), ShapeError> {
        assert!(self.append_checkpoint.is_none());
        self.append_checkpoint = Some(self.glyphs.keys.len());
        Ok(())
    }

    fn commit_append_shaping(&mut self) {
        self.append_checkpoint = None;
    }

    fn rollback_append_shaping(&mut self) {
        if let Some(len) = self.append_checkpoint.take() {
            self.glyphs.keys.truncate(len);
            self.glyphs.ids.retain(|_, glyph| (glyph.0 as usize) < len);
        }
    }

    fn shape_glyph(&mut self, request: GlyphRequest<'_>) -> Result<ShapedGlyph, ShapeError> {
        let key = GlyphKey {
            ch: request.character,
            size_bits: request.style.font_size.to_bits(),
            bold: request.style.font_weight >= 600,
            italic: !matches!(request.style.font_slant, FontSlant::Normal),
        };

        let face = self.face(key);
        let advance = face
            .metrics(request.character, request.style.font_size)
            .advance_width;
        let line = face
            .horizontal_line_metrics(request.style.font_size)
            .unwrap_or(fontdue::LineMetrics {
                ascent: request.style.font_size * 0.8,
                descent: -request.style.font_size * 0.2,
                line_gap: 0.0,
                new_line_size: request.style.font_size,
            });
        let advance = advance.max(0.0);
        let metrics = FontMetrics {
            ascent: line.ascent.max(0.0),
            descent: (-line.descent).max(0.0),
        };

        let glyphs = &mut self.glyphs;
        if let Some(&glyph) = glyphs.ids.get(&key) {
            return Ok(ShapedGlyph::new(glyph, advance, metrics));
        }
        let glyph = GlyphHandle(glyphs.keys.len() as u32);
        if glyphs.keys.len() <= glyph.0 as usize {
            glyphs.keys.resize(glyph.0 as usize + 1, key);
        }
        glyphs.keys[glyph.0 as usize] = key;
        glyphs.ids.insert(key, glyph);
        Ok(ShapedGlyph::new(glyph, advance, metrics))
    }
}
