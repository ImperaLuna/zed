use anyhow::{Context as _, Ok, Result};
use collections::HashMap;
use cosmic_text::{
    Attrs, AttrsList, Family, Font as CosmicTextFont, FontFeatures as CosmicFontFeatures,
    FontSystem, ShapeBuffer, ShapeLine,
};
use gpui::{
    Bounds, DevicePixels, Font, FontFeatures, FontId, FontMetrics, FontRun, GlyphId, LineLayout,
    Pixels, PlatformTextSystem, RenderGlyphParams, SUBPIXEL_VARIANTS_X, SUBPIXEL_VARIANTS_Y,
    ShapedGlyph, ShapedRun, SharedString, Size, TextRenderingMode, point, size,
};

use itertools::Itertools;
use parking_lot::{Mutex, RwLock};
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    sync::{Arc, OnceLock},
    thread::JoinHandle,
};
use swash::{
    scale::{Render, ScaleContext, Source, StrikeWith},
    zeno::{Format, Vector},
};

pub struct CosmicTextSystem {
    state: OnceLock<RwLock<CosmicTextSystemState>>,
    /// Setting up the fonts takes a while, most of all on a cold start, so
    /// `new` starts it on its own thread and the first text call waits for
    /// it. Meanwhile the platform connects and sets up the GPU.
    loading: Mutex<Option<LoadingFonts>>,
}

struct LoadingFonts {
    thread: JoinHandle<(FontSystem, Option<OnDemandFonts>)>,
    system_font_fallback: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FontKey {
    family: SharedString,
    features: FontFeatures,
}

impl FontKey {
    fn new(family: SharedString, features: FontFeatures) -> Self {
        Self { family, features }
    }
}

struct CosmicTextSystemState {
    font_system: FontSystem,
    scratch: ShapeBuffer,
    swash_scale_context: ScaleContext,
    /// Contains all already loaded fonts, including all faces. Indexed by `FontId`.
    loaded_fonts: Vec<LoadedFont>,
    /// Caches the `FontId`s associated with a specific family to avoid iterating the font database
    /// for every font face in a family.
    font_ids_by_family_cache: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    system_font_fallback: String,
    /// `Some` when fonts are read only once text needs them; `None` when
    /// every system font was read up front.
    on_demand: Option<OnDemandFonts>,
}

struct LoadedFont {
    font: Arc<CosmicTextFont>,
    features: CosmicFontFeatures,
    is_known_emoji_font: bool,
}

impl CosmicTextSystem {
    pub fn new(system_font_fallback: &str) -> Self {
        let spawned = std::thread::Builder::new()
            .name("gpui-load-fonts".to_string())
            .spawn(|| {
                let fonts = start_fonts();
                crate::startup_phase("fonts-ready");
                fonts
            });
        match spawned {
            std::result::Result::Ok(thread) => Self {
                state: OnceLock::new(),
                loading: Mutex::new(Some(LoadingFonts {
                    thread,
                    system_font_fallback: system_font_fallback.to_string(),
                })),
            },
            Err(error) => {
                log::warn!("could not start the font loading thread: {error}");
                let (font_system, on_demand) = start_fonts();
                crate::startup_phase("fonts-ready");
                Self::with_font_system(font_system, on_demand, system_font_fallback)
            }
        }
    }

    pub fn new_without_system_fonts(system_font_fallback: &str) -> Self {
        let font_system = FontSystem::new_with_locale_and_db(
            "en-US".to_string(),
            cosmic_text::fontdb::Database::new(),
        );
        Self::with_font_system(font_system, None, system_font_fallback)
    }

    fn with_font_system(
        font_system: FontSystem,
        on_demand: Option<OnDemandFonts>,
        system_font_fallback: &str,
    ) -> Self {
        Self {
            state: OnceLock::from(RwLock::new(CosmicTextSystemState::new(
                font_system,
                on_demand,
                system_font_fallback.to_string(),
            ))),
            loading: Mutex::new(None),
        }
    }

    fn state(&self) -> &RwLock<CosmicTextSystemState> {
        self.state.get_or_init(|| {
            let loading = self
                .loading
                .lock()
                .take()
                .expect("fonts are either loading or loaded");
            let (font_system, on_demand) = loading.thread.join().unwrap_or_else(|_| {
                log::error!("loading the system fonts panicked; going on without them");
                let font_system = FontSystem::new_with_locale_and_db(
                    "en-US".to_string(),
                    cosmic_text::fontdb::Database::new(),
                );
                (font_system, None)
            });
            RwLock::new(CosmicTextSystemState::new(
                font_system,
                on_demand,
                loading.system_font_fallback,
            ))
        })
    }
}

impl CosmicTextSystemState {
    fn new(
        font_system: FontSystem,
        on_demand: Option<OnDemandFonts>,
        system_font_fallback: String,
    ) -> Self {
        Self {
            font_system,
            on_demand,
            scratch: ShapeBuffer::default(),
            swash_scale_context: ScaleContext::new(),
            loaded_fonts: Vec::new(),
            font_ids_by_family_cache: HashMap::default(),
            system_font_fallback,
        }
    }
}

impl PlatformTextSystem for CosmicTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.state().write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut state = self.state().write();
        let state = &mut *state;
        let mut result = state
            .font_system
            .db()
            .faces()
            .filter_map(|face| face.families.first().map(|family| family.0.clone()))
            .collect_vec();
        if let Some(on_demand) = &mut state.on_demand {
            result.extend(on_demand.family_names());
        }
        result.sort();
        result.dedup();
        result
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let mut state = self.state().write();
        let key = FontKey::new(font.family.clone(), font.features.clone());
        let candidates = if let Some(font_ids) = state.font_ids_by_family_cache.get(&key) {
            font_ids.as_slice()
        } else {
            let font_ids = state.load_family(&font.family, &font.features)?;
            state.font_ids_by_family_cache.insert(key.clone(), font_ids);
            state.font_ids_by_family_cache[&key].as_ref()
        };

        let ix = find_best_match(font, candidates, &state)?;

        Ok(candidates[ix])
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let metrics = self
            .state()
            .read()
            .loaded_font(font_id)
            .font
            .as_swash()
            .metrics(&[]);

        FontMetrics {
            units_per_em: metrics.units_per_em as u32,
            ascent: metrics.ascent,
            descent: -metrics.descent,
            line_gap: metrics.leading,
            underline_position: metrics.underline_offset,
            underline_thickness: metrics.stroke_size,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: Bounds {
                origin: point(0.0, 0.0),
                size: size(metrics.max_width, metrics.ascent + metrics.descent),
            },
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let lock = self.state().read();
        let glyph_metrics = lock.loaded_font(font_id).font.as_swash().glyph_metrics(&[]);
        let glyph_id = glyph_id.0 as u16;
        Ok(Bounds {
            origin: point(0.0, 0.0),
            size: size(
                glyph_metrics.advance_width(glyph_id),
                glyph_metrics.advance_height(glyph_id),
            ),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.state().read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.state().read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.state().write().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.state().write().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.state().write().layout_line(text, font_size, runs)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Subpixel
    }
}

impl CosmicTextSystemState {
    fn loaded_font(&self, font_id: FontId) -> &LoadedFont {
        &self.loaded_fonts[font_id.0]
    }

    #[profiling::function]
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let db = self.font_system.db_mut();
        for bytes in fonts {
            match bytes {
                Cow::Borrowed(embedded_font) => {
                    db.load_font_data(embedded_font.to_vec());
                }
                Cow::Owned(bytes) => {
                    db.load_font_data(bytes);
                }
            }
        }
        Ok(())
    }

    #[profiling::function]
    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
    ) -> Result<SmallVec<[FontId; 4]>> {
        let name = gpui::font_name_with_fallbacks(name, &self.system_font_fallback).to_string();
        if let Some(on_demand) = &mut self.on_demand {
            on_demand.load_family(&mut self.font_system, &name);
        }

        let families = self
            .font_system
            .db()
            .faces()
            .filter(|face| face.families.iter().any(|family| *name == family.0))
            .map(|face| (face.id, face.post_script_name.clone()))
            .collect::<SmallVec<[_; 4]>>();

        let mut loaded_font_ids = SmallVec::new();
        for (font_id, postscript_name) in families {
            let font = self
                .font_system
                .get_font(font_id, cosmic_text::Weight::NORMAL)
                .context("Could not load font")?;

            // HACK: To let the storybook run and render Windows caption icons. We should actually do better font fallback.
            let allowed_bad_font_names = [
                "SegoeFluentIcons", // NOTE: Segoe fluent icons postscript name is inconsistent
                "Segoe Fluent Icons",
            ];

            if font.as_swash().charmap().map('m') == 0
                && !allowed_bad_font_names.contains(&postscript_name.as_str())
            {
                self.font_system.db_mut().remove_face(font.id());
                continue;
            };

            let font_id = FontId(self.loaded_fonts.len());
            loaded_font_ids.push(font_id);
            self.loaded_fonts.push(LoadedFont {
                font,
                features: cosmic_font_features(features)?,
                is_known_emoji_font: check_is_known_emoji_font(&postscript_name),
            });
        }

        Ok(loaded_font_ids)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let glyph_metrics = self.loaded_font(font_id).font.as_swash().glyph_metrics(&[]);
        Ok(Size {
            width: glyph_metrics.advance_width(glyph_id.0 as u16),
            height: glyph_metrics.advance_height(glyph_id.0 as u16),
        })
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let glyph_id = self.loaded_font(font_id).font.as_swash().charmap().map(ch);
        if glyph_id == 0 {
            None
        } else {
            Some(GlyphId(glyph_id.into()))
        }
    }

    fn raster_bounds(&mut self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let image = self.render_glyph_image(params)?;
        Ok(Bounds {
            origin: point(image.placement.left.into(), (-image.placement.top).into()),
            size: size(image.placement.width.into(), image.placement.height.into()),
        })
    }

    #[profiling::function]
    fn rasterize_glyph(
        &mut self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        }

        let mut image = self.render_glyph_image(params)?;
        let bitmap_size = glyph_bounds.size;
        match image.content {
            swash::scale::image::Content::Color | swash::scale::image::Content::SubpixelMask => {
                // Convert from RGBA to BGRA.
                for pixel in image.data.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                Ok((bitmap_size, image.data))
            }
            swash::scale::image::Content::Mask => Ok((bitmap_size, image.data)),
        }
    }

    fn render_glyph_image(
        &mut self,
        params: &RenderGlyphParams,
    ) -> Result<swash::scale::image::Image> {
        let loaded_font = &self.loaded_fonts[params.font_id.0];
        let font_ref = loaded_font.font.as_swash();
        let pixel_size = f32::from(params.font_size);

        let subpixel_offset = Vector::new(
            params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32 / params.scale_factor,
            params.subpixel_variant.y as f32 / SUBPIXEL_VARIANTS_Y as f32 / params.scale_factor,
        );

        let mut scaler = self
            .swash_scale_context
            .builder(font_ref)
            .size(pixel_size * params.scale_factor)
            .hint(true)
            .build();

        let sources: &[Source] = if params.is_emoji {
            &[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ]
        } else {
            &[Source::Outline]
        };

        let mut renderer = Render::new(sources);
        if params.subpixel_rendering {
            // There seems to be a bug in Swash where the B and R values are swapped.
            renderer
                .format(Format::subpixel_bgra())
                .offset(subpixel_offset);
        } else {
            renderer.format(Format::Alpha).offset(subpixel_offset);
        }

        let glyph_id: u16 = params.glyph_id.0.try_into()?;
        renderer
            .render(&mut scaler, glyph_id)
            .with_context(|| format!("unable to render glyph via swash for {params:?}"))
    }

    /// This is used when cosmic_text has chosen a fallback font instead of using the requested
    /// font, typically to handle some unicode characters. When this happens, `loaded_fonts` may not
    /// yet have an entry for this fallback font, and so one is added.
    ///
    /// Note that callers shouldn't use this `FontId` somewhere that will retrieve the corresponding
    /// `LoadedFont.features`, as it will have an arbitrarily chosen or empty value. The only
    /// current use of this field is for the *input* of `layout_line`, and so it's fine to use
    /// `font_id_for_cosmic_id` when computing the *output* of `layout_line`.
    fn font_id_for_cosmic_id(&mut self, id: cosmic_text::fontdb::ID) -> Result<FontId> {
        if let Some(ix) = self
            .loaded_fonts
            .iter()
            .position(|loaded_font| loaded_font.font.id() == id)
        {
            Ok(FontId(ix))
        } else {
            let font = self
                .font_system
                .get_font(id, cosmic_text::Weight::NORMAL)
                .context("failed to get fallback font from cosmic-text font system")?;
            let face = self
                .font_system
                .db()
                .face(id)
                .context("fallback font face not found in cosmic-text database")?;

            let font_id = FontId(self.loaded_fonts.len());
            self.loaded_fonts.push(LoadedFont {
                font,
                features: CosmicFontFeatures::new(),
                is_known_emoji_font: check_is_known_emoji_font(&face.post_script_name),
            });

            Ok(font_id)
        }
    }

    #[profiling::function]
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        if let Some(on_demand) = &mut self.on_demand {
            on_demand.load_fonts_for_text(&mut self.font_system, text);
        }

        let mut attrs_list = AttrsList::new(&Attrs::new());
        let mut offs = 0;
        for run in font_runs {
            let loaded_font = self.loaded_font(run.font_id);
            let Some(face) = self.font_system.db().face(loaded_font.font.id()) else {
                log::warn!(
                    "font face not found in database for font_id {:?}",
                    run.font_id
                );
                offs += run.len;
                continue;
            };
            let Some(first_family) = face.families.first() else {
                log::warn!(
                    "font face has no family names for font_id {:?}",
                    run.font_id
                );
                offs += run.len;
                continue;
            };

            attrs_list.add_span(
                offs..(offs + run.len),
                &Attrs::new()
                    .metadata(run.font_id.0)
                    .family(Family::Name(&first_family.0))
                    .stretch(face.stretch)
                    .style(face.style)
                    .weight(face.weight)
                    .font_features(loaded_font.features.clone()),
            );
            offs += run.len;
        }

        let line = ShapeLine::new(
            &mut self.font_system,
            text,
            &attrs_list,
            cosmic_text::Shaping::Advanced,
            4,
        );
        let mut layout_lines = Vec::with_capacity(1);
        line.layout_to_buffer(
            &mut self.scratch,
            f32::from(font_size),
            None, // We do our own wrapping
            cosmic_text::Wrap::None,
            None,
            &mut layout_lines,
            None,
            cosmic_text::Hinting::Disabled,
        );

        let Some(layout) = layout_lines.first() else {
            return LineLayout {
                font_size,
                width: Pixels::ZERO,
                ascent: Pixels::ZERO,
                descent: Pixels::ZERO,
                runs: Vec::new(),
                len: text.len(),
            };
        };

        let mut runs: Vec<ShapedRun> = Vec::new();
        for glyph in &layout.glyphs {
            let mut font_id = FontId(glyph.metadata);
            let mut loaded_font = self.loaded_font(font_id);
            if loaded_font.font.id() != glyph.font_id {
                match self.font_id_for_cosmic_id(glyph.font_id) {
                    std::result::Result::Ok(resolved_id) => {
                        font_id = resolved_id;
                        loaded_font = self.loaded_font(font_id);
                    }
                    Err(error) => {
                        log::warn!(
                            "failed to resolve cosmic font id {:?}: {error:#}",
                            glyph.font_id
                        );
                        continue;
                    }
                }
            }
            let is_emoji = loaded_font.is_known_emoji_font;

            // HACK: Prevent crash caused by variation selectors.
            if glyph.glyph_id == 3 && is_emoji {
                continue;
            }

            let shaped_glyph = ShapedGlyph {
                id: GlyphId(glyph.glyph_id as u32),
                position: point(glyph.x.into(), glyph.y.into()),
                index: glyph.start,
                is_emoji,
            };

            if let Some(last_run) = runs
                .last_mut()
                .filter(|last_run| last_run.font_id == font_id)
            {
                last_run.glyphs.push(shaped_glyph);
            } else {
                runs.push(ShapedRun {
                    font_id,
                    glyphs: vec![shaped_glyph],
                });
            }
        }

        LineLayout {
            font_size,
            width: layout.w.into(),
            ascent: layout.max_ascent.into(),
            descent: layout.max_descent.into(),
            runs,
            len: text.len(),
        }
    }
}

#[cfg(feature = "font-kit")]
fn find_best_match(
    font: &Font,
    candidates: &[FontId],
    state: &CosmicTextSystemState,
) -> Result<usize> {
    let candidate_properties = candidates
        .iter()
        .map(|font_id| {
            let database_id = state.loaded_font(*font_id).font.id();
            let face_info = state
                .font_system
                .db()
                .face(database_id)
                .context("font face not found in database")?;
            Ok(face_info_into_properties(face_info))
        })
        .collect::<Result<SmallVec<[_; 4]>>>()?;

    let ix =
        font_kit::matching::find_best_match(&candidate_properties, &font_into_properties(font))
            .context("requested font family contains no font matching the other parameters")?;

    Ok(ix)
}

#[cfg(not(feature = "font-kit"))]
fn find_best_match(
    font: &Font,
    candidates: &[FontId],
    state: &CosmicTextSystemState,
) -> Result<usize> {
    if candidates.is_empty() {
        anyhow::bail!("requested font family contains no font matching the other parameters");
    }
    if candidates.len() == 1 {
        return Ok(0);
    }

    let target_weight = font.weight.0;
    let target_italic = matches!(
        font.style,
        gpui::FontStyle::Italic | gpui::FontStyle::Oblique
    );

    let mut best_index = 0;
    let mut best_score = u32::MAX;

    for (index, font_id) in candidates.iter().enumerate() {
        let database_id = state.loaded_font(*font_id).font.id();
        let face_info = state
            .font_system
            .db()
            .face(database_id)
            .context("font face not found in database")?;

        let is_italic = matches!(
            face_info.style,
            cosmic_text::Style::Italic | cosmic_text::Style::Oblique
        );
        let style_penalty: u32 = if is_italic == target_italic { 0 } else { 1000 };
        let weight_diff = (face_info.weight.0 as i32 - target_weight as i32).unsigned_abs();
        let score = style_penalty + weight_diff;

        if score < best_score {
            best_score = score;
            best_index = index;
        }
    }

    Ok(best_index)
}

fn cosmic_font_features(features: &FontFeatures) -> Result<CosmicFontFeatures> {
    let mut result = CosmicFontFeatures::new();
    for feature in features.0.iter() {
        let name_bytes: [u8; 4] = feature
            .0
            .as_bytes()
            .try_into()
            .context("Incorrect feature flag format")?;

        let tag = cosmic_text::FeatureTag::new(&name_bytes);

        result.set(tag, feature.1);
    }
    Ok(result)
}

#[cfg(feature = "font-kit")]
fn font_into_properties(font: &gpui::Font) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match font.style {
            gpui::FontStyle::Normal => font_kit::properties::Style::Normal,
            gpui::FontStyle::Italic => font_kit::properties::Style::Italic,
            gpui::FontStyle::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(font.weight.0),
        stretch: Default::default(),
    }
}

#[cfg(feature = "font-kit")]
fn face_info_into_properties(
    face_info: &cosmic_text::fontdb::FaceInfo,
) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match face_info.style {
            cosmic_text::Style::Normal => font_kit::properties::Style::Normal,
            cosmic_text::Style::Italic => font_kit::properties::Style::Italic,
            cosmic_text::Style::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(face_info.weight.0.into()),
        stretch: match face_info.stretch {
            cosmic_text::Stretch::Condensed => font_kit::properties::Stretch::CONDENSED,
            cosmic_text::Stretch::Expanded => font_kit::properties::Stretch::EXPANDED,
            cosmic_text::Stretch::ExtraCondensed => font_kit::properties::Stretch::EXTRA_CONDENSED,
            cosmic_text::Stretch::ExtraExpanded => font_kit::properties::Stretch::EXTRA_EXPANDED,
            cosmic_text::Stretch::Normal => font_kit::properties::Stretch::NORMAL,
            cosmic_text::Stretch::SemiCondensed => font_kit::properties::Stretch::SEMI_CONDENSED,
            cosmic_text::Stretch::SemiExpanded => font_kit::properties::Stretch::SEMI_EXPANDED,
            cosmic_text::Stretch::UltraCondensed => font_kit::properties::Stretch::ULTRA_CONDENSED,
            cosmic_text::Stretch::UltraExpanded => font_kit::properties::Stretch::ULTRA_EXPANDED,
        },
    }
}

fn check_is_known_emoji_font(postscript_name: &str) -> bool {
    // TODO: Include other common emoji fonts
    postscript_name == "NotoColorEmoji"
}

/// Sets up the fonts for a new text system: through fontconfig when it's
/// there, so no font is read until text needs it, otherwise by reading every
/// system font.
fn start_fonts() -> (FontSystem, Option<OnDemandFonts>) {
    #[cfg(feature = "fontconfig")]
    if let Some(fontconfig) = crate::fontconfig::Fontconfig::new() {
        let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_string());
        let font_system =
            FontSystem::new_with_locale_and_db(locale, cosmic_text::fontdb::Database::new());
        return (font_system, Some(OnDemandFonts::new(fontconfig)));
    }
    (FontSystem::new(), None)
}

/// Reads a font file the first time text needs it: the files of a family
/// when the family is asked for, and a file that draws a character when a
/// line holds a character no font read so far has.
#[cfg(feature = "fontconfig")]
struct OnDemandFonts {
    fontconfig: crate::fontconfig::Fontconfig,
    files_read: collections::HashSet<std::path::PathBuf>,
    families_looked_up: collections::HashSet<String>,
    chars_looked_up: collections::HashSet<char>,
}

#[cfg(feature = "fontconfig")]
impl OnDemandFonts {
    fn new(fontconfig: crate::fontconfig::Fontconfig) -> Self {
        Self {
            fontconfig,
            files_read: Default::default(),
            families_looked_up: Default::default(),
            chars_looked_up: Default::default(),
        }
    }

    fn load_family(&mut self, font_system: &mut FontSystem, family: &str) {
        if !self.families_looked_up.insert(family.to_string()) {
            return;
        }
        for file in self.fontconfig.family_files(family) {
            self.read_file(font_system, file);
        }
    }

    #[profiling::function]
    fn load_fonts_for_text(&mut self, font_system: &mut FontSystem, text: &str) {
        for ch in text.chars() {
            if needs_no_font(ch) || !self.chars_looked_up.insert(ch) {
                continue;
            }
            if font_system_has_char(font_system, ch) {
                continue;
            }
            if let Some(file) = self.fontconfig.file_for_char(ch) {
                self.read_file(font_system, file);
            }
        }
    }

    fn family_names(&mut self) -> Vec<String> {
        self.fontconfig.family_names()
    }

    fn read_file(&mut self, font_system: &mut FontSystem, file: std::path::PathBuf) {
        if self.files_read.contains(&file) {
            return;
        }
        if let Err(error) = font_system.db_mut().load_font_file(&file) {
            log::warn!("could not read font file {}: {error}", file.display());
        }
        self.files_read.insert(file);
    }
}

/// Without fontconfig every system font is read at start, so there is
/// nothing to read on demand.
#[cfg(not(feature = "fontconfig"))]
enum OnDemandFonts {}

#[cfg(not(feature = "fontconfig"))]
impl OnDemandFonts {
    fn load_family(&mut self, _: &mut FontSystem, _: &str) {
        match *self {}
    }

    fn load_fonts_for_text(&mut self, _: &mut FontSystem, _: &str) {
        match *self {}
    }

    fn family_names(&mut self) -> Vec<String> {
        match *self {}
    }
}

/// Whether any font read so far draws `ch`.
#[cfg(feature = "fontconfig")]
fn font_system_has_char(font_system: &mut FontSystem, ch: char) -> bool {
    let face_ids = font_system.db().faces().map(|face| face.id).collect_vec();
    face_ids.into_iter().any(|id| {
        font_system
            .get_font(id, cosmic_text::Weight::NORMAL)
            .is_some_and(|font| font.as_swash().charmap().map(ch) != 0)
    })
}

/// Characters that shape to nothing visible (controls, joiners, variation
/// selectors, tags), so looking up a font for them would read one for no
/// reason.
#[cfg(feature = "fontconfig")]
fn needs_no_font(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

#[cfg(all(test, feature = "fontconfig"))]
mod tests {
    use super::*;
    use gpui::{font, px};

    /// A family fontconfig knows, from a list that covers common desktops
    /// and CI images; `None` when none is installed.
    fn installed_ui_family(fontconfig: &mut crate::fontconfig::Fontconfig) -> Option<&'static str> {
        [
            "DejaVu Sans",
            "Noto Sans",
            "Cantarell",
            "Liberation Sans",
            "FreeSans",
        ]
        .into_iter()
        .find(|family| !fontconfig.family_files(family).is_empty())
    }

    #[test]
    fn reads_only_the_fonts_text_needs() {
        let Some(mut fontconfig) = crate::fontconfig::Fontconfig::new() else {
            return;
        };
        let Some(family) = installed_ui_family(&mut fontconfig) else {
            return;
        };
        let system = CosmicTextSystem::new(family);
        let font_id = system
            .font_id(&font(family))
            .expect("installed family resolves");

        let state = system.state().read();
        assert!(state.on_demand.is_some(), "fontconfig is used");
        let files_read = state
            .on_demand
            .as_ref()
            .map_or(0, |fonts| fonts.files_read.len());
        assert!(
            files_read <= fontconfig.family_files(family).len(),
            "only the asked family was read, not every system font"
        );
        drop(state);

        // Every character either draws from a font that has it, or no
        // installed font has it at all.
        let text = "Report 日本語 Ελληνικά 😀.pdf";
        let layout = system.layout_line(
            text,
            px(14.),
            &[FontRun {
                len: text.len(),
                font_id,
            }],
        );
        let shaped = layout
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(move |glyph| (run.font_id, glyph)));
        for (run_font, glyph) in shaped {
            let ch = text[glyph.index..]
                .chars()
                .next()
                .expect("glyph maps to a character");
            if ch == ' ' || fontconfig.file_for_char(ch).is_none() {
                continue;
            }
            assert!(
                system.glyph_for_char(run_font, ch).is_some(),
                "{ch:?} is drawn from a font that has it"
            );
        }
    }
}
