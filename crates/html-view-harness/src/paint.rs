//! A painter that puts the renderer's output into a window's pixel buffer.
//!
//! Everything the renderer draws arrives here in layout coordinates: filled
//! rectangles, and glyphs by id at a baseline origin.

use std::rc::Rc;

use html::render::Painter;
use kurbo::{Point, Rect};
use peniko::Color;

use crate::text::{FontShaper, GlyphRaster};

pub struct BufferPainter<'a> {
    buffer: &'a mut [u32],
    width: usize,
    height: usize,
    shaper: &'a FontShaper,
    clips: Vec<Rect>,
}

impl<'a> BufferPainter<'a> {
    pub fn new(buffer: &'a mut [u32], width: usize, height: usize, shaper: &'a FontShaper) -> Self {
        Self { buffer, width, height, shaper, clips: Vec::new() }
    }

    pub fn clear(&mut self, color: Color) {
        self.buffer.fill(pack(color.r, color.g, color.b));
    }

    fn clipped(&self, x: f64, y: f64) -> bool {
        self.clips.last().is_some_and(|clip| x < clip.x0 || x >= clip.x1 || y < clip.y0 || y >= clip.y1)
    }

    /// Blends one pixel, `coverage` running from clear to opaque.
    fn blend(&mut self, x: f64, y: f64, color: Color, coverage: u8) {
        if x < 0.0 || y < 0.0 || coverage == 0 || self.clipped(x, y) {
            return;
        }
        let (x, y) = (x as usize, y as usize);
        if x >= self.width || y >= self.height {
            return;
        }
        let alpha = u32::from(coverage) * u32::from(color.a) / 255;
        if alpha == 0 {
            return;
        }
        let index = y * self.width + x;
        let existing = self.buffer[index];
        let mix = |channel: u32, over: u8| ((channel * (255 - alpha)) + u32::from(over) * alpha) / 255;
        self.buffer[index] = pack(mix((existing >> 16) & 0xff, color.r) as u8, mix((existing >> 8) & 0xff, color.g) as u8, mix(existing & 0xff, color.b) as u8);
    }

    pub fn fill(&mut self, rect: Rect, color: Color) {
        let x0 = rect.x0.max(0.0).round() as usize;
        let y0 = rect.y0.max(0.0).round() as usize;
        let x1 = rect.x1.max(0.0).round().min(self.width as f64) as usize;
        let y1 = rect.y1.max(0.0).round().min(self.height as f64) as usize;
        for y in y0..y1 {
            for x in x0..x1 {
                self.blend(x as f64, y as f64, color, 255);
            }
        }
    }

    fn blit(&mut self, raster: &Rc<GlyphRaster>, origin: Point, color: Color) {
        for row in 0..raster.height {
            for column in 0..raster.width {
                let coverage = raster.coverage[row * raster.width + column];
                self.blend(origin.x + f64::from(raster.left) + column as f64, origin.y + f64::from(raster.top) + row as f64, color, coverage);
            }
        }
    }

    /// Draws a string in the harness's own voice -- status lines and labels,
    /// not document content.
    pub fn label(&mut self, text: &str, x: f64, baseline: f64, size: f32, color: Color) {
        let mut pen = x;
        for ch in text.chars() {
            let (advance, raster) = self.shaper.label_glyph(ch, size);
            self.blit(&raster, Point::new(pen, baseline), color);
            pen += f64::from(advance);
        }
    }
}

fn pack(red: u8, green: u8, blue: u8) -> u32 {
    (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
}

impl Painter for BufferPainter<'_> {
    fn push_clip(&mut self, rect: Rect) {
        let clip = self.clips.last().map_or(rect, |current| current.intersect(rect));
        self.clips.push(clip);
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        self.fill(rect, color);
    }

    fn draw_glyph(&mut self, glyph: html::layout::GlyphId, origin: Point) {
        self.draw_glyph_with_color(glyph, origin, 0x1a1a1aff);
    }

    fn draw_glyph_with_color(&mut self, glyph: html::layout::GlyphId, origin: Point, color: u32) {
        let Some(raster) = self.shaper.raster(glyph) else { return };
        let [red, green, blue, alpha] = color.to_be_bytes();
        self.blit(&raster, origin, Color::rgba8(red, green, blue, alpha));
    }

    fn draw_image(&mut self, _image: &peniko::Image, _hash: &[u8], rect: Rect) {
        self.fill(rect, Color::rgba8(0x99, 0x99, 0x99, 0x40));
    }

    fn draw_resource_image(&mut self, _image_idx: u32, _uri: Option<&str>, rect: Rect) {
        self.fill(rect, Color::rgba8(0x99, 0x99, 0x99, 0x40));
    }
}
