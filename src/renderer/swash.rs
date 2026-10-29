use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use imgref::ImgVec;
use log::debug;
use rgb::RGBA8;
use swash::FontRef;
use swash::scale::image::{Content, Image};
use swash::scale::{Render, ScaleContext, Source, StrikeWith};

use crate::renderer::{Renderer, Settings, TextAttrs, color_to_rgb, text_attrs};
use crate::terminal::Snapshot;
use crate::theme::Theme;

type CharVariant = (char, bool, bool);
type FontFace = (String, bool, bool);

const POWERLINE_NUDGE: f64 = 0.02;
const POWERLINE_SAMPLES: usize = 4;

const GLYPH_SOURCES: &[Source] = &[
    Source::ColorOutline(0),
    Source::ColorBitmap(StrikeWith::BestFit),
    Source::Outline,
    Source::Bitmap(StrikeWith::BestFit),
];

#[derive(Clone, Copy)]
struct CellLayout {
    x_l: usize,
    x_r: usize,
    y_t: usize,
    y_b: usize,
    baseline: i32,
    underline_y: usize,
}

struct RenderCell {
    ch: char,
    layout: CellLayout,
    attrs: TextAttrs,
    fg: RGBA8,
}

#[derive(Clone, Copy)]
enum BoxLineStyle {
    Light,
    Heavy,
}

#[derive(Clone, Copy)]
struct BoxLineGlyph {
    up: Option<BoxLineStyle>,
    right: Option<BoxLineStyle>,
    down: Option<BoxLineStyle>,
    left: Option<BoxLineStyle>,
}

pub struct SwashRenderer {
    font_families: Vec<String>,
    theme: Theme,
    pixel_width: usize,
    pixel_height: usize,
    font_aa_levels: u16,
    font_size: usize,
    col_width: f64,
    row_height: f64,
    underline_offset: f64,
    underline_thickness: f64,
    font_db: fontdb::Database,
    scale_context: ScaleContext,
    glyph_cache: HashMap<CharVariant, Option<Image>>,
    font_id_cache: HashMap<FontFace, Option<fontdb::ID>>,
    bold_is_bright: bool,
    hinting: bool,
}

fn get_font_id<T: AsRef<str> + std::fmt::Debug>(
    db: &fontdb::Database,
    families: &[T],
    weight: fontdb::Weight,
    style: fontdb::Style,
) -> Option<fontdb::ID> {
    debug!(
        "looking up font for families={:?}, weight={}, style={:?}",
        families, weight.0, style
    );

    let families: Vec<fontdb::Family> = families
        .iter()
        .map(|name| fontdb::Family::Name(name.as_ref()))
        .collect();

    let query = fontdb::Query {
        families: &families,
        weight,
        stretch: fontdb::Stretch::Normal,
        style,
    };

    let font_id = db.query(&query)?;

    debug!("found font with id={:?}", font_id);

    Some(font_id)
}

fn font_id_key(font_id: fontdb::ID) -> [u64; 2] {
    // fontdb::ID has no public raw representation. Swash only needs this
    // key to be stable within our ScaleContext, and DefaultHasher::new()
    // is deterministic (unlike RandomState). The second slot of the key
    // is swash's sub-font discriminator (e.g. variation instance), which
    // we don't use, so we leave it at 0.
    let mut hasher = DefaultHasher::new();
    font_id.hash(&mut hasher);
    [hasher.finish(), 0]
}

fn col_width(db: &fontdb::Database, family: &str, font_size: usize) -> Option<f64> {
    let font_id = get_font_id(db, &[family], fontdb::Weight::NORMAL, fontdb::Style::Normal)?;

    db.with_face_data(font_id, |font_data, face_index| {
        let font = FontRef::from_index(font_data, face_index as usize)?;
        let glyph_id = font.charmap().map('/');
        let metrics = font.glyph_metrics(&[]).scale(font_size as f32);

        Some(metrics.advance_width(glyph_id) as f64)
    })?
}

fn underline_metrics(db: &fontdb::Database, family: &str, font_size: usize) -> Option<(f64, f64)> {
    let font_id = get_font_id(db, &[family], fontdb::Weight::NORMAL, fontdb::Style::Normal)?;

    db.with_face_data(font_id, |font_data, face_index| {
        let font = FontRef::from_index(font_data, face_index as usize)?;
        let metrics = font.metrics(&[]).scale(font_size as f32);

        Some((metrics.underline_offset as f64, metrics.stroke_size as f64))
    })?
}

fn glyph_image_is_visible(img: &Image) -> bool {
    img.placement.width > 0 && img.placement.height > 0 && !img.data.is_empty()
}

fn font_weight(bold: bool) -> fontdb::Weight {
    if bold {
        fontdb::Weight::BOLD
    } else {
        fontdb::Weight::NORMAL
    }
}

fn font_style(italic: bool) -> fontdb::Style {
    if italic {
        fontdb::Style::Italic
    } else {
        fontdb::Style::Normal
    }
}

impl SwashRenderer {
    pub fn new(settings: Settings) -> Self {
        let col_width = col_width(&settings.font_db, &settings.text_family, settings.font_size)
            .expect("text_family is guaranteed to resolve by fonts::init");

        let (underline_offset, underline_thickness) =
            underline_metrics(&settings.font_db, &settings.text_family, settings.font_size)
                .expect("text_family is guaranteed to resolve by fonts::init");

        let (cols, rows) = settings.terminal_size;
        let row_height = (settings.font_size as f64) * settings.line_height;

        Self {
            font_db: settings.font_db,
            font_families: settings.font_families,
            theme: settings.theme,
            pixel_width: ((cols + 2) as f64 * col_width).round() as usize,
            pixel_height: ((rows + 1) as f64 * row_height).round() as usize,
            font_aa_levels: settings.font_aa_levels,
            font_size: settings.font_size,
            col_width,
            row_height,
            underline_offset,
            underline_thickness,
            scale_context: ScaleContext::new(),
            font_id_cache: HashMap::new(),
            glyph_cache: HashMap::new(),
            bold_is_bright: settings.bold_is_bright,
            hinting: settings.hinting,
        }
    }

    fn get_font_id(&mut self, name: &str, bold: bool, italic: bool) -> &Option<fontdb::ID> {
        self.font_id_cache
            .entry((name.to_owned(), bold, italic))
            .or_insert_with(|| {
                get_font_id(
                    &self.font_db,
                    &[name],
                    font_weight(bold),
                    font_style(italic),
                )
            })
    }

    fn ensure_glyph(&mut self, ch: char, bold: bool, italic: bool) {
        let key = (ch, bold, italic);

        if self.glyph_cache.contains_key(&key) {
            return;
        }

        let mut tried_font_ids = HashSet::new();

        if let Some(glyph) = self.rasterize_family_glyph(ch, bold, italic, &mut tried_font_ids) {
            self.glyph_cache.insert(key, Some(glyph));
            return;
        }

        if bold || italic {
            if let Some(glyph) = self.rasterize_family_glyph(ch, false, false, &mut tried_font_ids)
            {
                self.glyph_cache.insert(key, Some(glyph));
                return;
            }
        }

        if let Some(glyph) = self.rasterize_fallback_glyph(ch, bold, italic, &tried_font_ids) {
            self.glyph_cache.insert(key, Some(glyph));
            return;
        }

        if bold || italic {
            if let Some(glyph) = self.rasterize_fallback_glyph(ch, false, false, &tried_font_ids) {
                self.glyph_cache.insert(key, Some(glyph));
                return;
            }
        }

        self.glyph_cache.insert(key, None);
    }

    fn get_glyph(&self, ch: char, bold: bool, italic: bool) -> &Option<Image> {
        self.glyph_cache
            .get(&(ch, bold, italic))
            .expect("caller must invoke ensure_glyph first")
    }

    fn rasterize_family_glyph(
        &mut self,
        ch: char,
        bold: bool,
        italic: bool,
        tried_font_ids: &mut HashSet<fontdb::ID>,
    ) -> Option<Image> {
        let families = self.font_families.clone();

        for name in &families {
            let Some(font_id) = *self.get_font_id(name, bold, italic) else {
                continue;
            };

            tried_font_ids.insert(font_id);

            if let Some(glyph) = self.rasterize_font_glyph(font_id, ch) {
                return Some(glyph);
            }
        }

        None
    }

    fn rasterize_fallback_glyph(
        &mut self,
        ch: char,
        bold: bool,
        italic: bool,
        tried_font_ids: &HashSet<fontdb::ID>,
    ) -> Option<Image> {
        let weight = font_weight(bold);
        let style = font_style(italic);

        // Match resvg/usvg behavior: if configured families miss, try any
        // loaded face so system fonts and --font-dir fonts can cover unlisted scripts.
        let fallback_font_ids: Vec<_> = self
            .font_db
            .faces()
            .filter(|face| {
                !tried_font_ids.contains(&face.id) && face.weight == weight && face.style == style
            })
            .map(|face| face.id)
            .collect();

        for font_id in fallback_font_ids {
            if let Some(glyph) = self.rasterize_font_glyph(font_id, ch) {
                return Some(glyph);
            }
        }

        None
    }

    fn rasterize_font_glyph(&mut self, font_id: fontdb::ID, ch: char) -> Option<Image> {
        let font_size = self.font_size as f32;
        let hinting = self.hinting;
        let scale_context = &mut self.scale_context;

        self.font_db
            .with_face_data(font_id, |font_data, face_index| {
                let font = FontRef::from_index(font_data, face_index as usize)?;
                let glyph_id = font.charmap().map(ch);

                if glyph_id == 0 {
                    return None;
                }

                let mut scaler = scale_context
                    .builder_with_id(font, font_id_key(font_id))
                    .size(font_size)
                    .hint(hinting)
                    .build();

                // Swash returns an empty image when a mapped glyph is in a
                // table it can't decompose, most notably COLRv1. Drop that
                // result so the family fallback loop can try the next font.
                Render::new(GLYPH_SOURCES)
                    .render(&mut scaler, glyph_id)
                    .filter(glyph_image_is_visible)
            })?
    }

    fn new_frame(&self) -> Vec<RGBA8> {
        vec![self.theme.background.with_alpha(255); self.pixel_width * self.pixel_height]
    }

    fn cell_layout(
        &self,
        margin_l: f64,
        margin_t: usize,
        row: usize,
        col: usize,
        width: usize,
    ) -> CellLayout {
        let baseline =
            margin_t as f64 + self.font_size as f64 + (row as f64 * self.row_height).round();

        CellLayout {
            x_l: (margin_l + col as f64 * self.col_width).round() as usize,
            x_r: (margin_l + (col + width) as f64 * self.col_width).round() as usize,
            y_t: margin_t + (row as f64 * self.row_height).round() as usize,
            y_b: margin_t + ((row + 1) as f64 * self.row_height).round() as usize,
            baseline: baseline as i32,
            underline_y: (baseline - self.underline_offset).round() as usize,
        }
    }

    fn foreground(&self, attrs: &TextAttrs) -> RGBA8 {
        attrs
            .foreground
            .map(|color| color_to_rgb(&color, &self.theme))
            .unwrap_or(self.theme.foreground)
            .with_alpha(255)
    }

    fn paint_background(&self, buf: &mut [RGBA8], layout: CellLayout, attrs: &TextAttrs) {
        let Some(c) = &attrs.background else {
            return;
        };

        let c = color_to_rgb(c, &self.theme).with_alpha(255);

        for y in layout.y_t..layout.y_b {
            let idx = y * self.pixel_width;
            buf[idx + layout.x_l..idx + layout.x_r].fill(c);
        }
    }

    fn paint_underline(&self, buf: &mut [RGBA8], layout: CellLayout, fg: RGBA8, underline: bool) {
        if !underline {
            return;
        }

        let thickness = (self.underline_thickness.round() as usize).max(1);

        for dy in 0..thickness {
            let y = layout.underline_y + dy;
            let idx = y * self.pixel_width;
            buf[idx + layout.x_l..idx + layout.x_r].fill(fg);
        }
    }

    fn box_thickness(&self) -> usize {
        (self.underline_thickness.ceil() as usize).max(1)
    }

    fn line_thickness(&self, style: BoxLineStyle) -> usize {
        let light = self.box_thickness();

        match style {
            BoxLineStyle::Light => light,
            BoxLineStyle::Heavy => light.saturating_mul(2),
        }
    }

    fn paint_box_lines(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        glyph: BoxLineGlyph,
    ) {
        let cell_w = layout.x_r.saturating_sub(layout.x_l);
        let cell_h = layout.y_b.saturating_sub(layout.y_t);
        let light_w = self.box_thickness().min(cell_w);
        let light_h = self.box_thickness().min(cell_h);
        let center_x_l = layout.x_l + (cell_w.saturating_sub(light_w)) / 2;
        let center_x_r = center_x_l + light_w;
        let center_y_t = layout.y_t + (cell_h.saturating_sub(light_h)) / 2;
        let center_y_b = center_y_t + light_h;

        let stroke_x = |style| {
            let stroke_w = self.line_thickness(style).min(cell_w);
            let x_l = layout.x_l + (cell_w - stroke_w) / 2;

            (x_l, x_l + stroke_w)
        };

        let stroke_y = |style| {
            let stroke_h = self.line_thickness(style).min(cell_h);
            let y_t = layout.y_t + (cell_h - stroke_h) / 2;

            (y_t, y_t + stroke_h)
        };

        let up_x = glyph.up.map(stroke_x);
        let down_x = glyph.down.map(stroke_x);
        let right_y = glyph.right.map(stroke_y);
        let left_y = glyph.left.map(stroke_y);

        let vertical_l = [up_x, down_x]
            .into_iter()
            .flatten()
            .map(|(x_l, _)| x_l)
            .min();

        let vertical_r = [up_x, down_x]
            .into_iter()
            .flatten()
            .map(|(_, x_r)| x_r)
            .max();

        let horizontal_t = [right_y, left_y]
            .into_iter()
            .flatten()
            .map(|(y_t, _)| y_t)
            .min();

        let horizontal_b = [right_y, left_y]
            .into_iter()
            .flatten()
            .map(|(_, y_b)| y_b)
            .max();

        let rects = [
            up_x.map(|(v_l, v_r)| (v_l, layout.y_t, v_r, horizontal_b.unwrap_or(center_y_b))),
            right_y.map(|(h_t, h_b)| (vertical_l.unwrap_or(center_x_l), h_t, layout.x_r, h_b)),
            down_x.map(|(v_l, v_r)| (v_l, horizontal_t.unwrap_or(center_y_t), v_r, layout.y_b)),
            left_y.map(|(h_t, h_b)| (layout.x_l, h_t, vertical_r.unwrap_or(center_x_r), h_b)),
        ];

        self.paint_rects_once(buf, &rects, fg, faint);
    }

    // The arms of a box glyph overlap where they meet. Painting each covered
    // pixel once keeps faint (half-alpha) lines even instead of doubling up
    // into a brighter dot at every junction and cell centre.
    fn paint_rects_once(
        &self,
        buf: &mut [RGBA8],
        rects: &[Option<(usize, usize, usize, usize)>],
        fg: RGBA8,
        faint: bool,
    ) {
        let x_l = rects.iter().flatten().map(|r| r.0).min();
        let y_t = rects.iter().flatten().map(|r| r.1).min();
        let (Some(x_l), Some(y_t)) = (x_l, y_t) else {
            return;
        };
        let x_r = rects
            .iter()
            .flatten()
            .map(|r| r.2)
            .max()
            .unwrap_or(x_l)
            .min(self.pixel_width);
        let y_b = rects
            .iter()
            .flatten()
            .map(|r| r.3)
            .max()
            .unwrap_or(y_t)
            .min(self.pixel_height);
        let ratio = if faint { 127 } else { 255 };

        for y in y_t..y_b {
            for x in x_l..x_r {
                let covered = rects
                    .iter()
                    .flatten()
                    .any(|&(l, t, r, b)| (l..r).contains(&x) && (t..b).contains(&y));

                if covered {
                    let idx = y * self.pixel_width + x;
                    buf[idx] = blend_straight_alpha(fg, buf[idx], ratio);
                }
            }
        }
    }

    fn paint_double_lines(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        [up, right, down, left]: [bool; 4],
    ) {
        let cell_w = layout.x_r.saturating_sub(layout.x_l);
        let cell_h = layout.y_b.saturating_sub(layout.y_t);
        let t = self.box_thickness();
        let center_x = layout.x_l + cell_w.saturating_sub(t) / 2;
        let center_y = layout.y_t + cell_h.saturating_sub(t) / 2;

        // Two strokes one stroke-width either side of the light centre line, so
        // double lines stay on the same grid as light and heavy ones.
        let near_x = (center_x.saturating_sub(t), center_x);
        let far_x = (center_x + t, center_x + 2 * t);
        let near_y = (center_y.saturating_sub(t), center_y);
        let far_y = (center_y + t, center_y + 2 * t);
        let horizontal = right || left;
        let vertical = up || down;

        if horizontal && !vertical {
            let rects = [near_y, far_y].map(|(y_t, y_b)| Some((layout.x_l, y_t, layout.x_r, y_b)));
            self.paint_rects_once(buf, &rects, fg, faint);
            return;
        }

        if vertical && !horizontal {
            let rects = [near_x, far_x].map(|(x_l, x_r)| Some((x_l, layout.y_t, x_r, layout.y_b)));
            self.paint_rects_once(buf, &rects, fg, faint);
            return;
        }

        // A corner: the stroke on the outside of the turn runs to the far edge of
        // the other outside stroke; the inside one stops at the inside stroke.
        let (outer_y, inner_y) = if down {
            (near_y, far_y)
        } else {
            (far_y, near_y)
        };
        let (outer_x, inner_x) = if right {
            (near_x, far_x)
        } else {
            (far_x, near_x)
        };

        let mut rects = [None; 4];

        for (at, ((h_t, h_b), (v_l, v_r))) in [(outer_y, outer_x), (inner_y, inner_x)]
            .into_iter()
            .enumerate()
        {
            let (h_l, h_r) = if right {
                (v_l, layout.x_r)
            } else {
                (layout.x_l, v_r)
            };
            let (v_t, v_b) = if down {
                (h_t, layout.y_b)
            } else {
                (layout.y_t, h_b)
            };

            rects[at * 2] = Some((h_l, h_t, h_r, h_b));
            rects[at * 2 + 1] = Some((v_l, v_t, v_r, v_b));
        }

        self.paint_rects_once(buf, &rects, fg, faint);
    }

    fn paint_box_arc(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        (sx, sy): (f64, f64),
    ) {
        let cell_w = layout.x_r.saturating_sub(layout.x_l);
        let cell_h = layout.y_b.saturating_sub(layout.y_t);

        if cell_w == 0 || cell_h == 0 {
            return;
        }

        let t = self.box_thickness().min(cell_w).min(cell_h);
        let half = t as f64 / 2.0;

        // The same centre lines as the light straight strokes, so the arc's ends
        // meet ─ and │ in the neighbouring cells exactly.
        let xc = (layout.x_l + (cell_w - t) / 2) as f64 + half;
        let yc = (layout.y_t + (cell_h - t) / 2) as f64 + half;
        let to_x = if sx > 0.0 {
            layout.x_r as f64 - xc
        } else {
            xc - layout.x_l as f64
        };
        let to_y = if sy > 0.0 {
            layout.y_b as f64 - yc
        } else {
            yc - layout.y_t as f64
        };
        let r = to_x.min(to_y);
        let (ax, ay) = (xc + sx * r, yc + sy * r);

        let covers = |px: f64, py: f64| {
            let past_x = (px - ax) * sx >= 0.0;
            let past_y = (py - ay) * sy >= 0.0;

            if past_y && !past_x {
                return (px - xc).abs() <= half;
            }

            if past_x && !past_y {
                return (py - yc).abs() <= half;
            }

            if past_x || past_y {
                return false;
            }

            ((px - ax).hypot(py - ay) - r).abs() <= half
        };

        let x_l = layout.x_l.min(self.pixel_width);
        let x_r = layout.x_r.min(self.pixel_width);
        let y_t = layout.y_t.min(self.pixel_height);
        let y_b = layout.y_b.min(self.pixel_height);
        let samples = POWERLINE_SAMPLES * POWERLINE_SAMPLES;
        let max_alpha = if faint { 127 } else { 255 };

        for y in y_t..y_b {
            for x in x_l..x_r {
                let mut coverage = 0;

                for sy_i in 0..POWERLINE_SAMPLES {
                    for sx_i in 0..POWERLINE_SAMPLES {
                        let px = x as f64 + (sx_i as f64 + 0.5) / POWERLINE_SAMPLES as f64;
                        let py = y as f64 + (sy_i as f64 + 0.5) / POWERLINE_SAMPLES as f64;

                        if covers(px, py) {
                            coverage += 1;
                        }
                    }
                }

                if coverage == 0 {
                    continue;
                }

                let ratio = ((coverage * max_alpha + samples / 2) / samples) as u8;
                let idx = y * self.pixel_width + x;
                buf[idx] = blend_straight_alpha(fg, buf[idx], ratio);
            }
        }
    }

    fn paint_powerline_symbol(
        &self,
        buf: &mut [RGBA8],
        ch: char,
        layout: CellLayout,
        attrs: &TextAttrs,
        fg: RGBA8,
    ) -> bool {
        match ch as u32 {
            // powerline right full triangle
            0xE0B0 => self.paint_powerline_filled_triangle(
                buf,
                layout,
                fg,
                attrs.faint,
                [(0.0, 0.0), (1.0, 0.5), (0.0, 1.0)],
            ),

            // powerline right bracket
            0xE0B1 => self.paint_powerline_stroke_segments(
                buf,
                layout,
                fg,
                attrs.faint,
                [((0.0, 0.0), (1.0, 0.5)), ((1.0, 0.5), (0.0, 1.0))],
            ),

            // powerline left full triangle
            0xE0B2 => self.paint_powerline_filled_triangle(
                buf,
                layout,
                fg,
                attrs.faint,
                [(1.0, 0.0), (0.0, 0.5), (1.0, 1.0)],
            ),

            // powerline left bracket
            0xE0B3 => self.paint_powerline_stroke_segments(
                buf,
                layout,
                fg,
                attrs.faint,
                [((1.0, 0.0), (0.0, 0.5)), ((0.0, 0.5), (1.0, 1.0))],
            ),

            // nf-ple-right_half_circle_thick
            0xE0B4 => self.paint_powerline_filled_cap(buf, layout, fg, attrs.faint, true),

            // nf-ple-right_half_circle_thin
            0xE0B5 => self.paint_powerline_stroked_cap(buf, layout, fg, attrs.faint, true),

            // nf-ple-left_half_circle_thick
            0xE0B6 => self.paint_powerline_filled_cap(buf, layout, fg, attrs.faint, false),

            // nf-ple-left_half_circle_thin
            0xE0B7 => self.paint_powerline_stroked_cap(buf, layout, fg, attrs.faint, false),

            // nf-ple-lower_left_triangle
            0xE0B8 => self.paint_powerline_filled_triangle(
                buf,
                layout,
                fg,
                attrs.faint,
                [(0.0, 1.0), (0.0, 0.0), (1.0, 1.0)],
            ),

            // nf-ple-backslash_separator + redundant
            0xE0B9 | 0xE0BF => self.paint_powerline_stroke_segments(
                buf,
                layout,
                fg,
                attrs.faint,
                [((0.0, 0.0), (1.0, 1.0))],
            ),

            // nf-ple-lower_right_triangle
            0xE0BA => self.paint_powerline_filled_triangle(
                buf,
                layout,
                fg,
                attrs.faint,
                [(1.0, 1.0), (1.0, 0.0), (0.0, 1.0)],
            ),

            // nf-ple-forwardslash_separator + redundant
            0xE0BB | 0xE0BD => self.paint_powerline_stroke_segments(
                buf,
                layout,
                fg,
                attrs.faint,
                [((0.0, 1.0), (1.0, 0.0))],
            ),

            // nf-ple-upper_left_triangle
            0xE0BC => self.paint_powerline_filled_triangle(
                buf,
                layout,
                fg,
                attrs.faint,
                [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)],
            ),

            // nf-ple-upper_right_triangle
            0xE0BE => self.paint_powerline_filled_triangle(
                buf,
                layout,
                fg,
                attrs.faint,
                [(1.0, 0.0), (1.0, 1.0), (0.0, 0.0)],
            ),

            _ => return false,
        }

        true
    }

    fn paint_powerline_filled_triangle(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        points: [(f64, f64); 3],
    ) {
        self.paint_powerline_samples(buf, layout, fg, faint, |x, y, _, _| {
            point_in_triangle((x, y), points)
        });
    }

    fn paint_powerline_stroke_segments<const N: usize>(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        segments: [((f64, f64), (f64, f64)); N],
    ) {
        let (draw_l, draw_w, _, cell_h) = self.powerline_bounds(layout);
        let stroke_radius = self.powerline_stroke_width() / 2.0;

        self.paint_powerline_samples(buf, layout, fg, faint, |_, _, px, py| {
            segments.iter().any(|&(a, b)| {
                // Stroke widths are measured in pixels, so diagonal and curved
                // Powerline strokes match box-drawing line thickness.
                let a = (draw_l + a.0 * draw_w, layout.y_t as f64 + a.1 * cell_h);
                let b = (draw_l + b.0 * draw_w, layout.y_t as f64 + b.1 * cell_h);

                distance_to_segment((px, py), a, b) <= stroke_radius
            })
        });
    }

    fn paint_powerline_filled_cap(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        right: bool,
    ) {
        self.paint_powerline_samples(buf, layout, fg, faint, |x, y, _, _| {
            // Half-disk from an ellipse centered on the left or right cell edge.
            let dx = if right { x } else { 1.0 - x };
            let dy = (y - 0.5) / 0.5;

            dx >= 0.0 && dx * dx + dy * dy <= 1.0
        });
    }

    fn paint_powerline_stroked_cap(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        right: bool,
    ) {
        let (draw_l, draw_w, _, cell_h) = self.powerline_bounds(layout);
        let stroke_width = self.powerline_stroke_width();
        let outer_rx = draw_w;
        let outer_ry = cell_h / 2.0;
        // The stroked cap is the outer half-ellipse minus a smaller inner one.
        let inner_rx = (outer_rx - stroke_width).max(0.0);
        let inner_ry = (outer_ry - stroke_width).max(0.0);
        let cx = if right { draw_l } else { draw_l + draw_w };
        let cy = layout.y_t as f64 + outer_ry;

        self.paint_powerline_samples(buf, layout, fg, faint, |_, _, px, py| {
            let dx = (px - cx).abs();
            let dy = (py - cy).abs();
            let inside_outer = ellipse_contains(dx, dy, outer_rx, outer_ry);
            let inside_inner = ellipse_contains(dx, dy, inner_rx, inner_ry);

            inside_outer && !inside_inner
        });
    }

    fn paint_powerline_samples(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        covers: impl Fn(f64, f64, f64, f64) -> bool,
    ) {
        let (draw_l, draw_w, cell_w, cell_h) = self.powerline_bounds(layout);

        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }

        let x_l = draw_l.floor().max(0.0) as usize;
        let x_r = (draw_l + draw_w).ceil().min(self.pixel_width as f64) as usize;
        let y_t = layout.y_t.min(self.pixel_height);
        let y_b = layout.y_b.min(self.pixel_height);
        let samples = POWERLINE_SAMPLES * POWERLINE_SAMPLES;
        let max_alpha = if faint { 127 } else { 255 };

        for y in y_t..y_b {
            for x in x_l..x_r {
                let mut coverage = 0;

                for sy in 0..POWERLINE_SAMPLES {
                    for sx in 0..POWERLINE_SAMPLES {
                        // Small fixed supersampling gives stable anti-aliased edges
                        // without pulling in another rasterizer.
                        let px = x as f64 + (sx as f64 + 0.5) / POWERLINE_SAMPLES as f64;
                        let py = y as f64 + (sy as f64 + 0.5) / POWERLINE_SAMPLES as f64;
                        let nx = (px - draw_l) / draw_w;
                        let ny = (py - layout.y_t as f64) / cell_h;

                        if covers(nx, ny, px, py) {
                            coverage += 1;
                        }
                    }
                }

                if coverage == 0 {
                    continue;
                }

                let ratio = ((coverage * max_alpha + samples / 2) / samples) as u8;
                let idx = y * self.pixel_width + x;
                buf[idx] = blend_straight_alpha(fg, buf[idx], ratio);
            }
        }
    }

    fn powerline_bounds(&self, layout: CellLayout) -> (f64, f64, f64, f64) {
        let cell_w = layout.x_r.saturating_sub(layout.x_l) as f64;
        let cell_h = layout.y_b.saturating_sub(layout.y_t) as f64;
        let nudge = cell_w * POWERLINE_NUDGE;
        let draw_l = layout.x_l as f64 - nudge;
        let draw_w = cell_w + nudge * 2.0;

        (draw_l, draw_w, cell_w, cell_h)
    }

    fn powerline_stroke_width(&self) -> f64 {
        self.box_thickness() as f64
    }

    fn paint_mosaic_symbol(
        &self,
        buf: &mut [RGBA8],
        ch: char,
        layout: CellLayout,
        attrs: &TextAttrs,
        fg: RGBA8,
    ) -> bool {
        let cp = ch as u32;
        let full = (layout.x_l, layout.y_t, layout.x_r, layout.y_b);
        let x = |n, d| split(layout.x_l, layout.x_r, n, d);
        let y = |n, d| split(layout.y_t, layout.y_b, n, d);
        let unit_x = |n| x(n, 8);
        let unit_y = |n| y(n, 8);
        let half_x = x(1, 2);
        let half_y = y(1, 2);

        use BoxLineStyle::{Heavy, Light};

        macro_rules! box_lines {
            ($up:expr, $right:expr, $down:expr, $left:expr) => {
                self.paint_box_lines(
                    buf,
                    layout,
                    fg,
                    attrs.faint,
                    BoxLineGlyph {
                        up: $up,
                        right: $right,
                        down: $down,
                        left: $left,
                    },
                )
            };
        }

        match cp {
            // box drawings light/heavy horizontal and vertical
            0x2500 => box_lines!(None, Some(Light), None, Some(Light)),
            0x2501 => box_lines!(None, Some(Heavy), None, Some(Heavy)),
            0x2502 => box_lines!(Some(Light), None, Some(Light), None),
            0x2503 => box_lines!(Some(Heavy), None, Some(Heavy), None),

            // box drawings light/heavy corners
            0x250C => box_lines!(None, Some(Light), Some(Light), None),
            0x250F => box_lines!(None, Some(Heavy), Some(Heavy), None),
            0x2510 => box_lines!(None, None, Some(Light), Some(Light)),
            0x2513 => box_lines!(None, None, Some(Heavy), Some(Heavy)),
            0x2514 => box_lines!(Some(Light), Some(Light), None, None),
            0x2517 => box_lines!(Some(Heavy), Some(Heavy), None, None),
            0x2518 => box_lines!(Some(Light), None, None, Some(Light)),
            0x251B => box_lines!(Some(Heavy), None, None, Some(Heavy)),

            // box drawings mixed junctions used by Rich tables
            0x2521 => box_lines!(Some(Heavy), Some(Heavy), Some(Light), None),
            0x2529 => box_lines!(Some(Heavy), None, Some(Light), Some(Heavy)),
            0x2533 => box_lines!(None, Some(Heavy), Some(Heavy), Some(Heavy)),
            0x2534 => box_lines!(Some(Light), Some(Light), None, Some(Light)),
            0x2547 => box_lines!(Some(Heavy), Some(Heavy), Some(Light), Some(Heavy)),

            // box drawings light/heavy tees and crosses
            0x251C => box_lines!(Some(Light), Some(Light), Some(Light), None),
            0x2523 => box_lines!(Some(Heavy), Some(Heavy), Some(Heavy), None),
            0x2524 => box_lines!(Some(Light), None, Some(Light), Some(Light)),
            0x252B => box_lines!(Some(Heavy), None, Some(Heavy), Some(Heavy)),
            0x252C => box_lines!(None, Some(Light), Some(Light), Some(Light)),
            0x253B => box_lines!(Some(Heavy), Some(Heavy), None, Some(Heavy)),
            0x253C => box_lines!(Some(Light), Some(Light), Some(Light), Some(Light)),
            0x254B => box_lines!(Some(Heavy), Some(Heavy), Some(Heavy), Some(Heavy)),

            // box drawings double lines and corners
            0x2550 => {
                self.paint_double_lines(buf, layout, fg, attrs.faint, [false, true, false, true])
            }
            0x2551 => {
                self.paint_double_lines(buf, layout, fg, attrs.faint, [true, false, true, false])
            }
            0x2554 => {
                self.paint_double_lines(buf, layout, fg, attrs.faint, [false, true, true, false])
            }
            0x2557 => {
                self.paint_double_lines(buf, layout, fg, attrs.faint, [false, false, true, true])
            }
            0x255A => {
                self.paint_double_lines(buf, layout, fg, attrs.faint, [true, true, false, false])
            }
            0x255D => {
                self.paint_double_lines(buf, layout, fg, attrs.faint, [true, false, false, true])
            }

            // box drawings light arcs ╭╮╯╰
            0x256D => self.paint_box_arc(buf, layout, fg, attrs.faint, (1.0, 1.0)),
            0x256E => self.paint_box_arc(buf, layout, fg, attrs.faint, (-1.0, 1.0)),
            0x256F => self.paint_box_arc(buf, layout, fg, attrs.faint, (-1.0, -1.0)),
            0x2570 => self.paint_box_arc(buf, layout, fg, attrs.faint, (1.0, -1.0)),

            // box drawings light/heavy half-lines
            0x2574 => box_lines!(None, None, None, Some(Light)),
            0x2575 => box_lines!(Some(Light), None, None, None),
            0x2576 => box_lines!(None, Some(Light), None, None),
            0x2577 => box_lines!(None, None, Some(Light), None),
            0x2578 => box_lines!(None, None, None, Some(Heavy)),
            0x2579 => box_lines!(Some(Heavy), None, None, None),
            0x257A => box_lines!(None, Some(Heavy), None, None),
            0x257B => box_lines!(None, None, Some(Heavy), None),

            // upper half block
            0x2580 => self.paint_cell_rect(
                buf,
                (layout.x_l, layout.y_t, layout.x_r, half_y),
                fg,
                attrs.faint,
            ),

            // lower N eighths blocks ▁▂▃▄▅▆▇█ (n=8 places top edge at y_t)
            0x2581..=0x2588 => {
                let n = (cp - 0x2580) as usize;

                self.paint_cell_rect(
                    buf,
                    (layout.x_l, unit_y(8 - n), layout.x_r, layout.y_b),
                    fg,
                    attrs.faint,
                );
            }

            // left N eighths blocks ▉▊▋▌▍▎▏
            0x2589..=0x258F => {
                let n = (cp - 0x2588) as usize;

                self.paint_cell_rect(
                    buf,
                    (layout.x_l, layout.y_t, unit_x(8 - n), layout.y_b),
                    fg,
                    attrs.faint,
                );
            }

            // right half block
            0x2590 => self.paint_cell_rect(
                buf,
                (half_x, layout.y_t, layout.x_r, layout.y_b),
                fg,
                attrs.faint,
            ),

            // light, medium, dark shade ░▒▓
            0x2591..=0x2593 => {
                let n = (cp - 0x2590) as u8;
                let ratio = if attrs.faint { 32 * n } else { 64 * n };

                self.paint_cell_rect_alpha(buf, full, fg, ratio);
            }

            // upper one eighth block
            0x2594 => self.paint_cell_rect(
                buf,
                (layout.x_l, layout.y_t, layout.x_r, unit_y(1)),
                fg,
                attrs.faint,
            ),

            // right one eighth block
            0x2595 => self.paint_cell_rect(
                buf,
                (unit_x(7), layout.y_t, layout.x_r, layout.y_b),
                fg,
                attrs.faint,
            ),

            // quadrant blocks ▖▗▘▙▚▛▜▝▞▟ (Unicode order doesn't match any
            // quadrant-bit pattern, so look up each combination)
            0x2596..=0x259F => {
                // Bits, top-to-bottom, left-to-right: 0b1=UL 0b10=UR 0b100=LL 0b1000=LR
                const QUADRANTS: [u8; 10] = [
                    0b0100, // ▖ lower left
                    0b1000, // ▗ lower right
                    0b0001, // ▘ upper left
                    0b1101, // ▙ ul + ll + lr
                    0b1001, // ▚ ul + lr
                    0b0111, // ▛ ul + ur + ll
                    0b1011, // ▜ ul + ur + lr
                    0b0010, // ▝ upper right
                    0b0110, // ▞ ur + ll
                    0b1110, // ▟ ur + ll + lr
                ];

                let mask = QUADRANTS[(cp - 0x2596) as usize];

                self.paint_quadrants(buf, layout, fg, attrs.faint, mask);
            }

            // black square, rendered as a centered half-height mosaic block
            0x25A0 => self.paint_cell_rect(
                buf,
                (layout.x_l, unit_y(2), layout.x_r, unit_y(6)),
                fg,
                attrs.faint,
            ),

            cp => {
                if let Some(mask) = sextant_mask(cp) {
                    self.paint_sextants(buf, layout, fg, attrs.faint, mask);
                } else if let Some(mask) = octant_mask(cp) {
                    self.paint_octants(buf, layout, fg, attrs.faint, mask);
                } else {
                    return false;
                }
            }
        }

        true
    }

    fn paint_quadrants(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        mask: u8,
    ) {
        let half_x = split(layout.x_l, layout.x_r, 1, 2);
        let half_y = split(layout.y_t, layout.y_b, 1, 2);

        if (mask & 0b0001) != 0 {
            self.paint_cell_rect(buf, (layout.x_l, layout.y_t, half_x, half_y), fg, faint);
        }

        if (mask & 0b0010) != 0 {
            self.paint_cell_rect(buf, (half_x, layout.y_t, layout.x_r, half_y), fg, faint);
        }

        if (mask & 0b0100) != 0 {
            self.paint_cell_rect(buf, (layout.x_l, half_y, half_x, layout.y_b), fg, faint);
        }

        if (mask & 0b1000) != 0 {
            self.paint_cell_rect(buf, (half_x, half_y, layout.x_r, layout.y_b), fg, faint);
        }
    }

    fn paint_sextants(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        mask: u8,
    ) {
        let x_mid = split(layout.x_l, layout.x_r, 1, 2);
        let y_1 = split(layout.y_t, layout.y_b, 1, 3);
        let y_2 = split(layout.y_t, layout.y_b, 2, 3);

        if (mask & 0b000001) != 0 {
            self.paint_cell_rect(buf, (layout.x_l, layout.y_t, x_mid, y_1), fg, faint);
        }

        if (mask & 0b000010) != 0 {
            self.paint_cell_rect(buf, (x_mid, layout.y_t, layout.x_r, y_1), fg, faint);
        }

        if (mask & 0b000100) != 0 {
            self.paint_cell_rect(buf, (layout.x_l, y_1, x_mid, y_2), fg, faint);
        }

        if (mask & 0b001000) != 0 {
            self.paint_cell_rect(buf, (x_mid, y_1, layout.x_r, y_2), fg, faint);
        }

        if (mask & 0b010000) != 0 {
            self.paint_cell_rect(buf, (layout.x_l, y_2, x_mid, layout.y_b), fg, faint);
        }

        if (mask & 0b100000) != 0 {
            self.paint_cell_rect(buf, (x_mid, y_2, layout.x_r, layout.y_b), fg, faint);
        }
    }

    fn paint_octants(
        &self,
        buf: &mut [RGBA8],
        layout: CellLayout,
        fg: RGBA8,
        faint: bool,
        mask: u8,
    ) {
        let xs = [layout.x_l, split(layout.x_l, layout.x_r, 1, 2), layout.x_r];
        let ys = [
            layout.y_t,
            split(layout.y_t, layout.y_b, 1, 4),
            split(layout.y_t, layout.y_b, 2, 4),
            split(layout.y_t, layout.y_b, 3, 4),
            layout.y_b,
        ];

        for bit in 0..8 {
            if mask & (1 << bit) != 0 {
                let (col, row) = (bit % 2, bit / 2);
                self.paint_cell_rect(buf, (xs[col], ys[row], xs[col + 1], ys[row + 1]), fg, faint);
            }
        }
    }

    fn paint_cell_rect(
        &self,
        buf: &mut [RGBA8],
        rect: (usize, usize, usize, usize),
        fg: RGBA8,
        faint: bool,
    ) {
        self.paint_cell_rect_alpha(buf, rect, fg, if faint { 127 } else { 255 });
    }

    fn paint_cell_rect_alpha(
        &self,
        buf: &mut [RGBA8],
        (x_l, y_t, x_r, y_b): (usize, usize, usize, usize),
        fg: RGBA8,
        ratio: u8,
    ) {
        if x_r <= x_l || y_b <= y_t {
            return;
        }

        let x_l = x_l.min(self.pixel_width);
        let x_r = x_r.min(self.pixel_width);
        let y_t = y_t.min(self.pixel_height);
        let y_b = y_b.min(self.pixel_height);

        for y in y_t..y_b {
            for x in x_l..x_r {
                let idx = y * self.pixel_width + x;
                let bg = buf[idx];

                buf[idx] = blend_straight_alpha(fg, bg, ratio);
            }
        }
    }

    fn paint_glyph(
        &mut self,
        buf: &mut [RGBA8],
        ch: char,
        layout: CellLayout,
        attrs: &TextAttrs,
        fg: RGBA8,
    ) {
        self.ensure_glyph(ch, attrs.bold, attrs.italic);
        let glyph = self.get_glyph(ch, attrs.bold, attrs.italic);

        let Some(glyph) = glyph.as_ref() else {
            return;
        };

        let placement = glyph.placement;
        let width = placement.width as usize;
        let height = placement.height as usize;
        let y_offset = layout.baseline - placement.top;
        let x_offset = layout.x_l as i32 + placement.left;

        match glyph.content {
            Content::Mask => {
                self.paint_image(buf, width, height, x_offset, y_offset, |bx, by, bg| {
                    let mut ratio =
                        quantize_alpha(glyph.data[by * width + bx], self.font_aa_levels);

                    if attrs.faint {
                        ratio = (ratio as f32 * 0.5) as u8;
                    }

                    blend_straight_alpha(fg, bg, ratio)
                });
            }

            Content::Color => {
                // Swash returns straight RGBA for color bitmap strikes (CBDT/sbix)
                // but premultiplied RGBA for layered color outlines (COLR/CPAL).
                let premultiplied = matches!(glyph.source, Source::ColorOutline(_));

                self.paint_image(buf, width, height, x_offset, y_offset, |bx, by, bg| {
                    let src_idx = (by * width + bx) * 4;

                    let mut src = RGBA8::new(
                        glyph.data[src_idx],
                        glyph.data[src_idx + 1],
                        glyph.data[src_idx + 2],
                        glyph.data[src_idx + 3],
                    );

                    if attrs.faint {
                        src = fade_color(src, premultiplied);
                    }

                    if premultiplied {
                        blend_premultiplied_alpha(src, bg)
                    } else {
                        blend_straight_alpha(src, bg, src.a)
                    }
                });
            }

            Content::SubpixelMask => {
                // We never request subpixel output from swash; Render defaults
                // to Format::Alpha, which produces Content::Mask for outlines.
                unreachable!("swash renderer does not request subpixel glyph masks");
            }
        }
    }

    fn paint_image(
        &self,
        buf: &mut [RGBA8],
        width: usize,
        height: usize,
        x_offset: i32,
        y_offset: i32,
        mut paint: impl FnMut(usize, usize, RGBA8) -> RGBA8,
    ) {
        for by in 0..height {
            let y = y_offset + by as i32;

            if y < 0 || y >= self.pixel_height as i32 {
                continue;
            }

            for bx in 0..width {
                let x = x_offset + bx as i32;

                if x < 0 || x >= self.pixel_width as i32 {
                    continue;
                }

                let idx = (y as usize) * self.pixel_width + (x as usize);
                buf[idx] = paint(bx, by, buf[idx]);
            }
        }
    }
}

fn quantize_alpha(alpha: u8, levels: u16) -> u8 {
    if levels >= crate::FULL_FONT_AA_LEVELS {
        return alpha;
    }

    let steps = levels.max(2) - 1;
    let bucket = ((alpha as u16 * steps + 127) / 255).min(steps);

    ((bucket * 255 + steps / 2) / steps) as u8
}

fn blend_straight_alpha(fg: RGBA8, bg: RGBA8, ratio: u8) -> RGBA8 {
    let ratio = ratio as u16;

    RGBA8::new(
        ((bg.r as u16) * (255 - ratio) / 255) as u8 + ((fg.r as u16) * ratio / 255) as u8,
        ((bg.g as u16) * (255 - ratio) / 255) as u8 + ((fg.g as u16) * ratio / 255) as u8,
        ((bg.b as u16) * (255 - ratio) / 255) as u8 + ((fg.b as u16) * ratio / 255) as u8,
        255,
    )
}

fn blend_premultiplied_alpha(fg: RGBA8, bg: RGBA8) -> RGBA8 {
    let inverse = 255 - fg.a as u16;

    RGBA8::new(
        (fg.r as u16 + (bg.r as u16) * inverse / 255).min(255) as u8,
        (fg.g as u16 + (bg.g as u16) * inverse / 255).min(255) as u8,
        (fg.b as u16 + (bg.b as u16) * inverse / 255).min(255) as u8,
        255,
    )
}

fn fade_color(mut color: RGBA8, premultiplied: bool) -> RGBA8 {
    if premultiplied {
        color.r /= 2;
        color.g /= 2;
        color.b /= 2;
    }

    color.a /= 2;

    color
}

fn split(start: usize, end: usize, numerator: usize, denominator: usize) -> usize {
    start + ((end - start) * numerator + denominator / 2) / denominator
}

fn point_in_triangle(p: (f64, f64), points: [(f64, f64); 3]) -> bool {
    let d1 = edge_sign(p, points[0], points[1]);
    let d2 = edge_sign(p, points[1], points[2]);
    let d3 = edge_sign(p, points[2], points[0]);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;

    !(has_neg && has_pos)
}

fn edge_sign((px, py): (f64, f64), (ax, ay): (f64, f64), (bx, by): (f64, f64)) -> f64 {
    (px - bx) * (ay - by) - (ax - bx) * (py - by)
}

fn distance_to_segment((px, py): (f64, f64), (ax, ay): (f64, f64), (bx, by): (f64, f64)) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    if len_sq == 0.0 {
        return ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
    }

    let t = (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0);
    let x = ax + t * dx;
    let y = ay + t * dy;

    ((px - x).powi(2) + (py - y).powi(2)).sqrt()
}

fn ellipse_contains(dx: f64, dy: f64, rx: f64, ry: f64) -> bool {
    if rx <= 0.0 || ry <= 0.0 {
        return false;
    }

    (dx / rx).powi(2) + (dy / ry).powi(2) <= 1.0
}

// Every 2x4 octant pattern and the code point that draws it (Unicode 16 and
// 17 octants plus the older block elements with the same shape), sorted by code
// point. Bit n is octant n + 1, numbered left to right, top to bottom.
const OCTANTS: [(u32, u8); 255] = [
    (0x2580, 0b00001111),
    (0x2582, 0b11000000),
    (0x2584, 0b11110000),
    (0x2586, 0b11111100),
    (0x2588, 0b11111111),
    (0x258C, 0b01010101),
    (0x2590, 0b10101010),
    (0x2596, 0b01010000),
    (0x2597, 0b10100000),
    (0x2598, 0b00000101),
    (0x2599, 0b11110101),
    (0x259A, 0b10100101),
    (0x259B, 0b01011111),
    (0x259C, 0b10101111),
    (0x259D, 0b00001010),
    (0x259E, 0b01011010),
    (0x259F, 0b11111010),
    (0x1CD00, 0b00000100),
    (0x1CD01, 0b00000110),
    (0x1CD02, 0b00000111),
    (0x1CD03, 0b00001000),
    (0x1CD04, 0b00001001),
    (0x1CD05, 0b00001011),
    (0x1CD06, 0b00001100),
    (0x1CD07, 0b00001101),
    (0x1CD08, 0b00001110),
    (0x1CD09, 0b00010000),
    (0x1CD0A, 0b00010001),
    (0x1CD0B, 0b00010010),
    (0x1CD0C, 0b00010011),
    (0x1CD0D, 0b00010101),
    (0x1CD0E, 0b00010110),
    (0x1CD0F, 0b00010111),
    (0x1CD10, 0b00011000),
    (0x1CD11, 0b00011001),
    (0x1CD12, 0b00011010),
    (0x1CD13, 0b00011011),
    (0x1CD14, 0b00011100),
    (0x1CD15, 0b00011101),
    (0x1CD16, 0b00011110),
    (0x1CD17, 0b00011111),
    (0x1CD18, 0b00100000),
    (0x1CD19, 0b00100001),
    (0x1CD1A, 0b00100010),
    (0x1CD1B, 0b00100011),
    (0x1CD1C, 0b00100100),
    (0x1CD1D, 0b00100101),
    (0x1CD1E, 0b00100110),
    (0x1CD1F, 0b00100111),
    (0x1CD20, 0b00101001),
    (0x1CD21, 0b00101010),
    (0x1CD22, 0b00101011),
    (0x1CD23, 0b00101100),
    (0x1CD24, 0b00101101),
    (0x1CD25, 0b00101110),
    (0x1CD26, 0b00101111),
    (0x1CD27, 0b00110000),
    (0x1CD28, 0b00110001),
    (0x1CD29, 0b00110010),
    (0x1CD2A, 0b00110011),
    (0x1CD2B, 0b00110100),
    (0x1CD2C, 0b00110101),
    (0x1CD2D, 0b00110110),
    (0x1CD2E, 0b00110111),
    (0x1CD2F, 0b00111000),
    (0x1CD30, 0b00111001),
    (0x1CD31, 0b00111010),
    (0x1CD32, 0b00111011),
    (0x1CD33, 0b00111100),
    (0x1CD34, 0b00111101),
    (0x1CD35, 0b00111110),
    (0x1CD36, 0b01000001),
    (0x1CD37, 0b01000010),
    (0x1CD38, 0b01000011),
    (0x1CD39, 0b01000100),
    (0x1CD3A, 0b01000101),
    (0x1CD3B, 0b01000110),
    (0x1CD3C, 0b01000111),
    (0x1CD3D, 0b01001000),
    (0x1CD3E, 0b01001001),
    (0x1CD3F, 0b01001010),
    (0x1CD40, 0b01001011),
    (0x1CD41, 0b01001100),
    (0x1CD42, 0b01001101),
    (0x1CD43, 0b01001110),
    (0x1CD44, 0b01001111),
    (0x1CD45, 0b01010001),
    (0x1CD46, 0b01010010),
    (0x1CD47, 0b01010011),
    (0x1CD48, 0b01010100),
    (0x1CD49, 0b01010110),
    (0x1CD4A, 0b01010111),
    (0x1CD4B, 0b01011000),
    (0x1CD4C, 0b01011001),
    (0x1CD4D, 0b01011011),
    (0x1CD4E, 0b01011100),
    (0x1CD4F, 0b01011101),
    (0x1CD50, 0b01011110),
    (0x1CD51, 0b01100000),
    (0x1CD52, 0b01100001),
    (0x1CD53, 0b01100010),
    (0x1CD54, 0b01100011),
    (0x1CD55, 0b01100100),
    (0x1CD56, 0b01100101),
    (0x1CD57, 0b01100110),
    (0x1CD58, 0b01100111),
    (0x1CD59, 0b01101000),
    (0x1CD5A, 0b01101001),
    (0x1CD5B, 0b01101010),
    (0x1CD5C, 0b01101011),
    (0x1CD5D, 0b01101100),
    (0x1CD5E, 0b01101101),
    (0x1CD5F, 0b01101110),
    (0x1CD60, 0b01101111),
    (0x1CD61, 0b01110000),
    (0x1CD62, 0b01110001),
    (0x1CD63, 0b01110010),
    (0x1CD64, 0b01110011),
    (0x1CD65, 0b01110100),
    (0x1CD66, 0b01110101),
    (0x1CD67, 0b01110110),
    (0x1CD68, 0b01110111),
    (0x1CD69, 0b01111000),
    (0x1CD6A, 0b01111001),
    (0x1CD6B, 0b01111010),
    (0x1CD6C, 0b01111011),
    (0x1CD6D, 0b01111100),
    (0x1CD6E, 0b01111101),
    (0x1CD6F, 0b01111110),
    (0x1CD70, 0b01111111),
    (0x1CD71, 0b10000001),
    (0x1CD72, 0b10000010),
    (0x1CD73, 0b10000011),
    (0x1CD74, 0b10000100),
    (0x1CD75, 0b10000101),
    (0x1CD76, 0b10000110),
    (0x1CD77, 0b10000111),
    (0x1CD78, 0b10001000),
    (0x1CD79, 0b10001001),
    (0x1CD7A, 0b10001010),
    (0x1CD7B, 0b10001011),
    (0x1CD7C, 0b10001100),
    (0x1CD7D, 0b10001101),
    (0x1CD7E, 0b10001110),
    (0x1CD7F, 0b10001111),
    (0x1CD80, 0b10010000),
    (0x1CD81, 0b10010001),
    (0x1CD82, 0b10010010),
    (0x1CD83, 0b10010011),
    (0x1CD84, 0b10010100),
    (0x1CD85, 0b10010101),
    (0x1CD86, 0b10010110),
    (0x1CD87, 0b10010111),
    (0x1CD88, 0b10011000),
    (0x1CD89, 0b10011001),
    (0x1CD8A, 0b10011010),
    (0x1CD8B, 0b10011011),
    (0x1CD8C, 0b10011100),
    (0x1CD8D, 0b10011101),
    (0x1CD8E, 0b10011110),
    (0x1CD8F, 0b10011111),
    (0x1CD90, 0b10100001),
    (0x1CD91, 0b10100010),
    (0x1CD92, 0b10100011),
    (0x1CD93, 0b10100100),
    (0x1CD94, 0b10100110),
    (0x1CD95, 0b10100111),
    (0x1CD96, 0b10101000),
    (0x1CD97, 0b10101001),
    (0x1CD98, 0b10101011),
    (0x1CD99, 0b10101100),
    (0x1CD9A, 0b10101101),
    (0x1CD9B, 0b10101110),
    (0x1CD9C, 0b10110000),
    (0x1CD9D, 0b10110001),
    (0x1CD9E, 0b10110010),
    (0x1CD9F, 0b10110011),
    (0x1CDA0, 0b10110100),
    (0x1CDA1, 0b10110101),
    (0x1CDA2, 0b10110110),
    (0x1CDA3, 0b10110111),
    (0x1CDA4, 0b10111000),
    (0x1CDA5, 0b10111001),
    (0x1CDA6, 0b10111010),
    (0x1CDA7, 0b10111011),
    (0x1CDA8, 0b10111100),
    (0x1CDA9, 0b10111101),
    (0x1CDAA, 0b10111110),
    (0x1CDAB, 0b10111111),
    (0x1CDAC, 0b11000001),
    (0x1CDAD, 0b11000010),
    (0x1CDAE, 0b11000011),
    (0x1CDAF, 0b11000100),
    (0x1CDB0, 0b11000101),
    (0x1CDB1, 0b11000110),
    (0x1CDB2, 0b11000111),
    (0x1CDB3, 0b11001000),
    (0x1CDB4, 0b11001001),
    (0x1CDB5, 0b11001010),
    (0x1CDB6, 0b11001011),
    (0x1CDB7, 0b11001100),
    (0x1CDB8, 0b11001101),
    (0x1CDB9, 0b11001110),
    (0x1CDBA, 0b11001111),
    (0x1CDBB, 0b11010000),
    (0x1CDBC, 0b11010001),
    (0x1CDBD, 0b11010010),
    (0x1CDBE, 0b11010011),
    (0x1CDBF, 0b11010100),
    (0x1CDC0, 0b11010101),
    (0x1CDC1, 0b11010110),
    (0x1CDC2, 0b11010111),
    (0x1CDC3, 0b11011000),
    (0x1CDC4, 0b11011001),
    (0x1CDC5, 0b11011010),
    (0x1CDC6, 0b11011011),
    (0x1CDC7, 0b11011100),
    (0x1CDC8, 0b11011101),
    (0x1CDC9, 0b11011110),
    (0x1CDCA, 0b11011111),
    (0x1CDCB, 0b11100000),
    (0x1CDCC, 0b11100001),
    (0x1CDCD, 0b11100010),
    (0x1CDCE, 0b11100011),
    (0x1CDCF, 0b11100100),
    (0x1CDD0, 0b11100101),
    (0x1CDD1, 0b11100110),
    (0x1CDD2, 0b11100111),
    (0x1CDD3, 0b11101000),
    (0x1CDD4, 0b11101001),
    (0x1CDD5, 0b11101010),
    (0x1CDD6, 0b11101011),
    (0x1CDD7, 0b11101100),
    (0x1CDD8, 0b11101101),
    (0x1CDD9, 0b11101110),
    (0x1CDDA, 0b11101111),
    (0x1CDDB, 0b11110001),
    (0x1CDDC, 0b11110010),
    (0x1CDDD, 0b11110011),
    (0x1CDDE, 0b11110100),
    (0x1CDDF, 0b11110110),
    (0x1CDE0, 0b11110111),
    (0x1CDE1, 0b11111000),
    (0x1CDE2, 0b11111001),
    (0x1CDE3, 0b11111011),
    (0x1CDE4, 0b11111101),
    (0x1CDE5, 0b11111110),
    (0x1CEA0, 0b10000000),
    (0x1CEA3, 0b01000000),
    (0x1CEA8, 0b00000001),
    (0x1CEAB, 0b00000010),
    (0x1FB82, 0b00000011),
    (0x1FB85, 0b00111111),
    (0x1FBE6, 0b00010100),
    (0x1FBE7, 0b00101000),
];

fn octant_mask(cp: u32) -> Option<u8> {
    OCTANTS
        .binary_search_by_key(&cp, |&(code, _)| code)
        .ok()
        .map(|index| OCTANTS[index].1)
}

fn sextant_mask(cp: u32) -> Option<u8> {
    if !(0x1FB00..=0x1FB3B).contains(&cp) {
        return None;
    }

    let offset = (cp - 0x1FB00) as u8;
    let shift = match offset / 20 {
        0 => 1,
        1 => 2,
        _ => 3,
    };

    Some(offset + shift)
}

impl Renderer for SwashRenderer {
    fn render(&mut self, snapshot: &Snapshot) -> ImgVec<RGBA8> {
        let mut buf = self.new_frame();
        let margin_l = self.col_width;
        let margin_t = (self.row_height / 2.0).round() as usize;
        let mut cells = Vec::new();

        for (row, line) in snapshot.lines.iter().enumerate() {
            let mut col = 0;

            for cell in line.cells() {
                let ch = cell.char();
                let cell_width = cell.width() as usize;
                let layout = self.cell_layout(margin_l, margin_t, row, col, cell_width);

                let attrs = text_attrs(
                    cell.pen(),
                    &snapshot.cursor,
                    col,
                    row,
                    &self.theme,
                    self.bold_is_bright,
                );

                let fg = self.foreground(&attrs);

                cells.push(RenderCell {
                    ch,
                    layout,
                    attrs,
                    fg,
                });

                col += cell_width;
            }
        }

        for cell in &cells {
            self.paint_background(&mut buf, cell.layout, &cell.attrs);
        }

        for cell in &cells {
            self.paint_underline(&mut buf, cell.layout, cell.fg, cell.attrs.underline);
        }

        for cell in &cells {
            if cell.ch != ' '
                && !self.paint_powerline_symbol(
                    &mut buf,
                    cell.ch,
                    cell.layout,
                    &cell.attrs,
                    cell.fg,
                )
                && !self.paint_mosaic_symbol(&mut buf, cell.ch, cell.layout, &cell.attrs, cell.fg)
            {
                self.paint_glyph(&mut buf, cell.ch, cell.layout, &cell.attrs, cell.fg);
            }
        }

        ImgVec::new(buf, self.pixel_width, self.pixel_height)
    }

    fn pixel_size(&self) -> (usize, usize) {
        (self.pixel_width, self.pixel_height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_image_visibility_rejects_empty_images() {
        let mut image = Image::new();

        assert!(!glyph_image_is_visible(&image));

        image.placement.width = 1;
        image.placement.height = 1;
        assert!(!glyph_image_is_visible(&image));

        image.data.push(255);
        assert!(glyph_image_is_visible(&image));
    }

    #[test]
    fn color_bitmap_edges_use_straight_alpha() {
        let bg = RGBA8::new(24, 24, 24, 255);
        let src = RGBA8::new(252, 215, 52, 2);

        assert_eq!(
            blend_straight_alpha(src, bg, src.a),
            RGBA8::new(24, 24, 23, 255)
        );
    }

    #[test]
    fn color_outline_edges_use_premultiplied_alpha() {
        let bg = RGBA8::new(24, 24, 24, 255);
        let src = RGBA8::new(2, 2, 0, 2);

        assert_eq!(
            blend_premultiplied_alpha(src, bg),
            RGBA8::new(25, 25, 23, 255)
        );
    }

    #[test]
    fn faint_color_preserves_alpha_representation() {
        let src = RGBA8::new(100, 80, 60, 40);

        assert_eq!(fade_color(src, false), RGBA8::new(100, 80, 60, 20));
        assert_eq!(fade_color(src, true), RGBA8::new(50, 40, 30, 20));
    }

    #[test]
    fn quantize_alpha_preserves_full_aa() {
        assert_eq!(quantize_alpha(0, crate::FULL_FONT_AA_LEVELS), 0);
        assert_eq!(quantize_alpha(37, crate::FULL_FONT_AA_LEVELS), 37);
        assert_eq!(quantize_alpha(255, crate::FULL_FONT_AA_LEVELS), 255);
    }

    #[test]
    fn quantize_alpha_maps_to_even_levels() {
        assert_eq!(quantize_alpha(0, 4), 0);
        assert_eq!(quantize_alpha(42, 4), 0);
        assert_eq!(quantize_alpha(43, 4), 85);
        assert_eq!(quantize_alpha(128, 4), 170);
        assert_eq!(quantize_alpha(255, 4), 255);
    }

    #[test]
    fn quantize_alpha_treats_two_levels_as_aliased() {
        assert_eq!(quantize_alpha(127, 2), 0);
        assert_eq!(quantize_alpha(128, 2), 255);
    }
}
