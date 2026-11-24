//! Android owns color emoji rasterization: modern system fonts use COLRv1,
//! which Swash cannot rasterize. Shaping and all other glyphs remain in cosmic-text.
use anyhow::{Result, ensure};
use gpui::{
    Bounds, DevicePixels, Font, FontId, FontMetrics, FontRun, GlyphId, Hsla, IsZero, LineLayout,
    Pixels, PlatformTextSystem, RenderGlyphParams, Size, TextRenderingMode, point, size,
};
use gpui_wgpu::CosmicTextSystem;
use parking_lot::Mutex;
use std::{borrow::Cow, collections::HashMap};

struct EmojiImage {
    bounds: Bounds<DevicePixels>,
    pixels: Vec<u8>,
}

pub(super) struct AndroidTextSystem {
    inner: CosmicTextSystem,
    pending: Mutex<HashMap<RenderGlyphParams, EmojiImage>>,
}

impl AndroidTextSystem {
    pub(super) fn new(inner: CosmicTextSystem) -> Self {
        Self {
            inner,
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn emoji_image(&self, params: &RenderGlyphParams) -> Result<Option<EmojiImage>> {
        if !params.is_emoji {
            return Ok(None);
        }
        let Some(data) = super::host::rasterize_emoji(
            params.glyph_id.0,
            f32::from(params.font_size) * params.scale_factor,
        )?
        else {
            return Ok(None);
        };
        ensure!(
            data.len() >= 4 && data[2] >= 0 && data[3] >= 0,
            "invalid emoji bitmap header"
        );
        let pixel_count = (data[2] as usize).checked_mul(data[3] as usize);
        ensure!(
            pixel_count == Some(data.len() - 4),
            "invalid emoji bitmap length"
        );
        let bounds = Bounds {
            origin: point(DevicePixels(data[0]), DevicePixels(data[1])),
            size: size(DevicePixels(data[2]), DevicePixels(data[3])),
        };
        let pixels = data[4..]
            .iter()
            .flat_map(|pixel| (*pixel as u32).to_le_bytes())
            .collect();
        Ok(Some(EmojiImage { bounds, pixels }))
    }
}

impl PlatformTextSystem for AndroidTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.inner.add_fonts(fonts)
    }
    fn all_font_names(&self) -> Vec<String> {
        self.inner.all_font_names()
    }
    fn font_id(&self, font: &Font) -> Result<FontId> {
        self.inner.font_id(font)
    }
    fn prewarm_fonts(&self, ids: &[FontId]) {
        self.inner.prewarm_fonts(ids);
    }
    fn font_metrics(&self, id: FontId) -> FontMetrics {
        self.inner.font_metrics(id)
    }
    fn typographic_bounds(&self, font: FontId, glyph: GlyphId) -> Result<Bounds<f32>> {
        self.inner.typographic_bounds(font, glyph)
    }
    fn advance(&self, font: FontId, glyph: GlyphId) -> Result<Size<f32>> {
        self.inner.advance(font, glyph)
    }
    fn glyph_for_char(&self, font: FontId, ch: char) -> Option<GlyphId> {
        self.inner.glyph_for_char(font, ch)
    }
    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        if let Some(image) = self.emoji_image(params)? {
            let bounds = image.bounds;
            if !bounds.is_zero() {
                self.pending.lock().insert(params.clone(), image);
            }
            Ok(bounds)
        } else {
            self.inner.glyph_raster_bounds(params)
        }
    }
    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        let pending = self.pending.lock().remove(params);
        let image = match pending {
            Some(image) => Some(image),
            None => self.emoji_image(params)?,
        };
        if let Some(image) = image {
            Ok((image.bounds.size, image.pixels))
        } else {
            self.inner.rasterize_glyph(params, bounds)
        }
    }
    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.inner.layout_line(text, font_size, runs)
    }
    fn recommended_rendering_mode(&self, font: FontId, size: Pixels) -> TextRenderingMode {
        self.inner.recommended_rendering_mode(font, size)
    }
    fn glyph_dilation_for_color(&self, color: Hsla) -> u8 {
        self.inner.glyph_dilation_for_color(color)
    }
}
