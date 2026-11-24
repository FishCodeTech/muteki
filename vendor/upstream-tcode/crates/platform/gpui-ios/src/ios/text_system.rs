//! CoreText-backed shaping and glyph rasterization.
//!
//! CoreText performs language-aware fallback, so a run styled with the iOS
//! system UI font automatically uses PingFang for Simplified Chinese glyphs.

use anyhow::{Result, anyhow};
use core_foundation::{
    attributed_string::CFMutableAttributedString,
    base::{CFRange, TCFType},
    number::CFNumber,
    string::CFString,
};
use core_graphics::{
    base::{CGFloat, CGGlyph, kCGImageAlphaPremultipliedLast},
    color_space::CGColorSpace,
    context::{CGContext, CGTextDrawingMode},
    geometry::CGPoint,
};
use core_text::{
    font::CTFont,
    font_descriptor::{
        SymbolicTraitAccessors, kCTFontSlantTrait, kCTFontWeightTrait, kCTFontWidthTrait,
    },
    line::CTLine,
    string_attributes::kCTFontAttributeName,
};
use font_kit::{
    font::Font as FontKitFont,
    handle::Handle,
    hinting::HintingOptions,
    properties::{
        Properties, Stretch as FontKitStretch, Style as FontKitStyle, Weight as FontKitWeight,
    },
    source::SystemSource,
    sources::mem::MemSource,
};
use gpui::{
    Bounds, DevicePixels, Font, FontFallbacks, FontFeatures, FontId, FontMetrics, FontRun,
    FontStyle, FontWeight, GlyphId, LineLayout, Pixels, PlatformTextSystem, RenderGlyphParams,
    SUBPIXEL_VARIANTS_X, ShapedGlyph, ShapedRun, Size, TextRenderingMode, point, px, size,
    swap_rgba_pa_to_bgra,
};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use pathfinder_geometry::{
    rect::{RectF, RectI},
    transform2d::Transform2F,
    vector::Vector2F,
};
use smallvec::SmallVec;
use std::{borrow::Cow, collections::HashMap, sync::Arc};

const CG_IMAGE_ALPHA_ONLY: u32 = 7;

/// iOS text implementation using the same CoreText shaping model as GPUI's
/// macOS backend.
pub(crate) struct IosTextSystem(RwLock<TextSystemState>);

#[derive(Clone, PartialEq, Eq, Hash)]
struct FontKey {
    family: gpui::SharedString,
    features: FontFeatures,
    fallbacks: Option<FontFallbacks>,
}

struct TextSystemState {
    memory_source: MemSource,
    system_source: SystemSource,
    fonts: Vec<FontKitFont>,
    selections: HashMap<Font, FontId>,
    ids_by_key: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    ids_by_postscript_name: HashMap<String, FontId>,
    postscript_names: HashMap<FontId, String>,
}

impl IosTextSystem {
    pub(crate) fn new() -> Self {
        Self(RwLock::new(TextSystemState {
            memory_source: MemSource::empty(),
            system_source: SystemSource::new(),
            fonts: Vec::new(),
            selections: HashMap::new(),
            ids_by_key: HashMap::new(),
            ids_by_postscript_name: HashMap::new(),
            postscript_names: HashMap::new(),
        }))
    }
}

impl PlatformTextSystem for IosTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut names = self
            .0
            .read()
            .memory_source
            .all_families()
            .unwrap_or_default();
        names.extend([
            ".AppleSystemUIFont".to_owned(),
            "PingFang SC".to_owned(),
            "Helvetica".to_owned(),
        ]);
        names.sort_unstable();
        names.dedup();
        names
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let lock = self.0.upgradable_read();
        if let Some(id) = lock.selections.get(font) {
            return Ok(*id);
        }

        let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
        let key = FontKey {
            family: font.family.clone(),
            features: font.features.clone(),
            fallbacks: font.fallbacks.clone(),
        };
        let candidates = if let Some(ids) = lock.ids_by_key.get(&key) {
            ids.clone()
        } else {
            let ids = lock.load_family(&font.family)?;
            lock.ids_by_key.insert(key, ids.clone());
            ids
        };
        let properties = candidates
            .iter()
            .map(|id| lenient_font_properties(&lock.fonts[id.0]))
            .collect::<SmallVec<[Properties; 4]>>();
        let index = font_kit::matching::find_best_match(
            &properties,
            &Properties {
                style: font_style(font.style),
                weight: font_weight(font.weight),
                stretch: Default::default(),
            },
        )?;
        let id = candidates[index];
        lock.selections.insert(font.clone(), id);
        Ok(id)
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let metrics = self.0.read().fonts[font_id.0].metrics();
        FontMetrics {
            units_per_em: metrics.units_per_em,
            ascent: metrics.ascent,
            descent: metrics.descent,
            line_gap: metrics.line_gap,
            underline_position: metrics.underline_position,
            underline_thickness: metrics.underline_thickness,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: rect_f_bounds(metrics.bounding_box),
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        Ok(rect_f_bounds(
            self.0.read().fonts[font_id.0].typographic_bounds(glyph_id.0)?,
        ))
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        Ok(vector_size(
            self.0.read().fonts[font_id.0].advance(glyph_id.0)?,
        ))
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().fonts[font_id.0]
            .glyph_for_char(ch)
            .map(GlyphId)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.read().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.read().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Grayscale
    }
}

impl TextSystemState {
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let handles = fonts
            .into_iter()
            .map(|bytes| match bytes {
                Cow::Borrowed(bytes) => {
                    let provider =
                        unsafe { core_graphics::data_provider::CGDataProvider::from_slice(bytes) };
                    let font = core_graphics::font::CGFont::from_data_provider(provider)
                        .map_err(|()| anyhow!("could not load embedded font"))?;
                    let font = font_kit_font(&font);
                    Ok(Handle::from_native(&font))
                }
                Cow::Owned(bytes) => Ok(Handle::from_memory(Arc::new(bytes), 0)),
            })
            .collect::<Result<Vec<_>>>()?;
        self.memory_source.add_fonts(handles.into_iter())?;
        Ok(())
    }

    fn load_family(&mut self, requested: &str) -> Result<SmallVec<[FontId; 4]>> {
        let requested = gpui::font_name_with_fallbacks(requested, ".AppleSystemUIFont");
        let family = self
            .memory_source
            .select_family_by_name(requested)
            .or_else(|_| self.system_source.select_family_by_name(requested))
            .or_else(|_| self.system_source.select_family_by_name("PingFang SC"))
            .or_else(|_| self.system_source.select_family_by_name("Helvetica"))?;

        let mut result = SmallVec::new();
        for handle in family.fonts() {
            let font = handle.load()?;
            if font.glyph_for_char('m').is_none() {
                continue;
            }
            let id = FontId(self.fonts.len());
            if let Some(name) = font.postscript_name() {
                self.ids_by_postscript_name.insert(name.clone(), id);
                self.postscript_names.insert(id, name);
            }
            self.fonts.push(font);
            result.push(id);
        }

        if result.is_empty() {
            return Err(anyhow!("iOS font family {requested:?} has no usable faces"));
        }
        Ok(result)
    }

    fn id_for_native_font(&mut self, font: CTFont) -> FontId {
        let name = font.postscript_name();
        if let Some(id) = self.ids_by_postscript_name.get(&name) {
            return *id;
        }

        let id = FontId(self.fonts.len());
        self.ids_by_postscript_name.insert(name.clone(), id);
        self.postscript_names.insert(id, name);
        self.fonts.push(font_kit_font(&font.copy_to_CGFont()));
        id
    }

    fn is_emoji(&self, id: FontId) -> bool {
        self.postscript_names
            .get(&id)
            .is_some_and(|name| name == "AppleColorEmoji" || name == ".AppleColorEmojiUI")
    }

    fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let bounds = self.fonts[params.font_id.0].raster_bounds(
            params.glyph_id.0,
            params.font_size.into(),
            Transform2F::from_scale(params.scale_factor),
            HintingOptions::None,
            font_kit::canvas::RasterizationOptions::GrayscaleAa,
        )?;
        Ok(rect_i_bounds(bounds).dilate(DevicePixels(1)))
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if bounds.size.width.0 <= 0 || bounds.size.height.0 <= 0 {
            return Err(anyhow!("glyph raster bounds are empty"));
        }

        let mut bitmap_size = bounds.size;
        if params.subpixel_variant.x > 0 {
            bitmap_size.width += DevicePixels(1);
        }
        if params.subpixel_variant.y > 0 {
            bitmap_size.height += DevicePixels(1);
        }

        let width = bitmap_size.width.0 as usize;
        let height = bitmap_size.height.0 as usize;
        let emoji = params.is_emoji;
        let bytes_per_pixel = if emoji { 4 } else { 1 };
        let mut bytes = vec![0; width * height * bytes_per_pixel];
        let color_space = if emoji {
            CGColorSpace::create_device_rgb()
        } else {
            CGColorSpace::create_device_gray()
        };
        let alpha = if emoji {
            kCGImageAlphaPremultipliedLast
        } else {
            CG_IMAGE_ALPHA_ONLY
        };
        let context = CGContext::create_bitmap_context(
            Some(bytes.as_mut_ptr().cast()),
            width,
            height,
            8,
            width * bytes_per_pixel,
            &color_space,
            alpha,
        );
        context.translate(
            -bounds.origin.x.0 as CGFloat,
            (bounds.origin.y.0 + bounds.size.height.0) as CGFloat,
        );
        context.scale(
            params.scale_factor as CGFloat,
            params.scale_factor as CGFloat,
        );
        context.set_text_drawing_mode(CGTextDrawingMode::CGTextFill);
        context.set_gray_fill_color(0.0, 1.0);
        context.set_allows_antialiasing(true);
        context.set_should_antialias(true);
        context.set_allows_font_subpixel_positioning(true);
        context.set_should_subpixel_position_fonts(true);
        context.set_allows_font_subpixel_quantization(false);
        context.set_should_subpixel_quantize_fonts(false);

        let shift = params
            .subpixel_variant
            .map(|value| value as f32 / SUBPIXEL_VARIANTS_X as f32);
        let native = self.fonts[params.font_id.0].native_font();
        // SAFETY: Both core-text versions wrap the same retained CTFontRef.
        let native = unsafe { CTFont::wrap_under_get_rule(native.as_concrete_TypeRef().cast()) };
        native
            .clone_with_font_size(f32::from(params.font_size) as CGFloat)
            .draw_glyphs(
                &[params.glyph_id.0 as CGGlyph],
                &[CGPoint::new(
                    (shift.x / params.scale_factor) as CGFloat,
                    (shift.y / params.scale_factor) as CGFloat,
                )],
                context,
            );

        if emoji {
            for pixel in bytes.chunks_exact_mut(4) {
                swap_rgba_pa_to_bgra(pixel);
            }
        }
        Ok((bitmap_size, bytes))
    }

    fn layout_line(&mut self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let mut attributed = CFMutableAttributedString::new();
        let mut remaining = text;
        let mut max_ascent = 0.0_f32;
        let mut max_descent = 0.0_f32;

        for (index, run) in runs.iter().enumerate() {
            let (text_run, rest) = remaining.split_at(run.len);
            remaining = rest;
            let utf16_start = attributed.char_len();
            attributed.replace_str(&CFString::new(text_run), CFRange::init(utf16_start, 0));
            let utf16_len = attributed.char_len() - utf16_start;
            let font = &self.fonts[run.font_id.0];
            let metrics = font.metrics();
            let scale = f32::from(font_size) / metrics.units_per_em as f32;
            max_ascent = max_ascent.max(metrics.ascent * scale);
            max_descent = max_descent.max(-metrics.descent * scale);

            // Alternating the least significant size bit prevents CoreText
            // from joining ligatures across GPUI font-run boundaries.
            let run_size = if index % 2 == 0 {
                f32::from(font_size).next_up()
            } else {
                f32::from(font_size)
            };
            let native = font.native_font().clone_with_font_size(run_size as CGFloat);
            unsafe {
                attributed.set_attribute(
                    CFRange::init(utf16_start, utf16_len),
                    kCTFontAttributeName,
                    &native,
                );
            }
        }

        let line = CTLine::new_with_attributed_string(attributed.as_concrete_TypeRef());
        let native_runs = line.glyph_runs();
        let mut shaped_runs = Vec::<ShapedRun>::with_capacity(native_runs.len() as usize);
        let mut converter = StringIndexConverter::new(text);
        for native_run in native_runs.into_iter() {
            let Some(attributes) = native_run.attributes() else {
                continue;
            };
            let native_font = unsafe { attributes.get(kCTFontAttributeName).downcast::<CTFont>() };
            let Some(native_font) = native_font else {
                continue;
            };
            let id = self.id_for_native_font(native_font);
            let is_emoji = self.is_emoji(id);
            let glyphs = match shaped_runs.last_mut() {
                Some(run) if run.font_id == id => &mut run.glyphs,
                _ => {
                    shaped_runs.push(ShapedRun {
                        font_id: id,
                        glyphs: Vec::with_capacity(native_run.glyph_count() as usize),
                    });
                    &mut shaped_runs
                        .last_mut()
                        .expect("run was just inserted")
                        .glyphs
                }
            };
            for ((glyph, position), utf16_index) in native_run
                .glyphs()
                .iter()
                .zip(native_run.positions().iter())
                .zip(native_run.string_indices().iter())
            {
                let utf16_index = usize::try_from(*utf16_index).unwrap_or(0);
                if converter.utf16 > utf16_index {
                    converter = StringIndexConverter::new(text);
                }
                converter.advance_to(utf16_index);
                glyphs.push(ShapedGlyph {
                    id: GlyphId(u32::from(*glyph)),
                    position: point(px(position.x as f32), px(position.y as f32)),
                    index: converter.utf8,
                    is_emoji,
                });
            }
        }

        let bounds = line.get_typographic_bounds();
        LineLayout {
            runs: shaped_runs,
            font_size,
            width: px(bounds.width as f32),
            ascent: px(max_ascent.max(bounds.ascent as f32)),
            descent: px(max_descent.max(bounds.descent as f32)),
            len: text.len(),
        }
    }
}

#[derive(Clone, Debug)]
struct StringIndexConverter<'a> {
    text: &'a str,
    utf8: usize,
    utf16: usize,
}

impl<'a> StringIndexConverter<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            utf8: 0,
            utf16: 0,
        }
    }

    fn advance_to(&mut self, target: usize) {
        for (offset, character) in self.text[self.utf8..].char_indices() {
            if self.utf16 >= target {
                self.utf8 += offset;
                return;
            }
            self.utf16 += character.len_utf16();
        }
        self.utf8 = self.text.len();
    }
}

fn font_kit_font(font: &core_graphics::font::CGFont) -> FontKitFont {
    let native = core_text::font::new_from_CGFont(font, 16.0);
    // SAFETY: font-kit uses core-text 21, which wraps the same CTFontRef as
    // core-text 22. The get rule retains it before `native` is dropped.
    let native = unsafe {
        font_kit::loaders::core_text::NativeFont::wrap_under_get_rule(
            native.as_concrete_TypeRef().cast(),
        )
    };
    // SAFETY: `native` owns a retained CoreText font.
    unsafe { FontKitFont::from_core_text_font_no_path(native) }
}

fn rect_f_bounds(rect: RectF) -> Bounds<f32> {
    Bounds::new(
        point(rect.origin_x(), rect.origin_y()),
        size(rect.width(), rect.height()),
    )
}

fn rect_i_bounds(rect: RectI) -> Bounds<DevicePixels> {
    Bounds::new(
        point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
        size(DevicePixels(rect.width()), DevicePixels(rect.height())),
    )
}

fn vector_size(vector: Vector2F) -> Size<f32> {
    size(vector.x(), vector.y())
}

fn font_weight(weight: FontWeight) -> FontKitWeight {
    FontKitWeight(weight.0)
}

fn font_style(style: FontStyle) -> FontKitStyle {
    match style {
        FontStyle::Normal => FontKitStyle::Normal,
        FontStyle::Italic => FontKitStyle::Italic,
        FontStyle::Oblique => FontKitStyle::Oblique,
    }
}

/// `zed-font-kit` delegates these lookups to `core-text`, whose trait
/// accessors unwrap every dictionary value. iOS system fonts legitimately
/// omit neutral traits such as slant, so read them leniently and use CoreText's
/// documented neutral value (`0`) when an entry is absent.
fn lenient_font_properties(font: &FontKitFont) -> Properties {
    let native = font.native_font();
    let traits = native.all_traits();
    // SAFETY: CoreText exports these process-lifetime `CFStringRef` keys.
    let (slant, weight, width) = unsafe {
        (
            trait_number(&traits, kCTFontSlantTrait).unwrap_or(0.0),
            trait_number(&traits, kCTFontWeightTrait).unwrap_or(0.0) as f32,
            trait_number(&traits, kCTFontWidthTrait).unwrap_or(0.0) as f32,
        )
    };
    let symbolic = native.symbolic_traits();

    Properties {
        style: if symbolic.is_italic() {
            FontKitStyle::Italic
        } else if slant > 0.0 {
            FontKitStyle::Oblique
        } else {
            FontKitStyle::Normal
        },
        weight: FontKitWeight(
            interpolated_index(weight, &[-0.7, -0.5, -0.23, 0.0, 0.2, 0.3, 0.4, 0.6, 0.8]) * 100.0
                + 100.0,
        ),
        stretch: FontKitStretch(interpolated_value(
            ((width + 1.0) * 4.0).clamp(0.0, 8.0),
            &[0.5, 0.625, 0.75, 0.875, 1.0, 1.125, 1.25, 1.5, 2.0],
        )),
    }
}

fn trait_number(
    traits: &core_text::font_descriptor::CTFontTraits,
    key: core_foundation::string::CFStringRef,
) -> Option<f64> {
    traits.get(key).downcast::<CFNumber>()?.to_f64()
}

fn interpolated_index(value: f32, mapping: &[f32]) -> f32 {
    if value <= mapping[0] {
        return 0.0;
    }
    let last = mapping.len() - 1;
    if value >= mapping[last] {
        return last as f32;
    }
    let upper = mapping.partition_point(|candidate| *candidate < value);
    let lower = upper - 1;
    let fraction = (value - mapping[lower]) / (mapping[upper] - mapping[lower]);
    lower as f32 + fraction
}

fn interpolated_value(index: f32, mapping: &[f32]) -> f32 {
    let lower = index.floor() as usize;
    let upper = index.ceil() as usize;
    mapping[lower] + (mapping[upper] - mapping[lower]) * index.fract()
}
