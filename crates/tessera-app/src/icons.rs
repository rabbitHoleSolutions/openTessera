//! Icons painted rather than typed, for buttons that have to look right at
//! any size.
//!
//! The transport's ⏮ ⏪ ⏩ ⏭ and the library's ▶ came from egui's emoji
//! fallback font, whose advance and baseline don't match the UI font's: at
//! a small chip scale they sat off centre, clipped against their buttons'
//! edges, and no two lined up. A shape painted from the button's own rect
//! is centred by construction and scales with it. The same goes for the
//! info and subtitle buttons, which were the letters "i" and "S" — a
//! circled i and a captioned screen are what people look for.

use egui::{pos2, vec2, Color32, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// ⓘ: what is this file?
    Info,
    /// A screen with two lines of captions.
    Subtitles,
    /// "A", painted so it sits in the same box as its neighbours.
    Audio,
    Play,
    /// Previous item: bar, then a left-pointing triangle.
    Prev,
    Next,
    /// Two left-pointing triangles: skip back.
    Back,
    Forward,
}

/// A button of exactly `size` with `icon` centred in it. `fill` overrides
/// the background, for a toggle that is on.
pub fn button(ui: &mut Ui, icon: Icon, size: Vec2, fill: Option<Color32>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        // The same visuals egui's own Button uses, so a painted button
        // hovers and presses like the text ones beside it.
        let v = ui.style().interact(&resp);
        let bg = fill.unwrap_or(v.weak_bg_fill);
        ui.painter().rect(rect.expand(v.expansion), v.rounding, bg, v.bg_stroke);
        paint(ui.painter(), icon, rect, v.fg_stroke.color, bg, 1.0 / ui.ctx().pixels_per_point());
    }
    resp
}

/// The size of a `.small()` text button in the current style, for an icon
/// button that sits in a row of them.
pub fn small_size(ui: &Ui) -> Vec2 {
    let h = ui.text_style_height(&egui::TextStyle::Button);
    vec2(h + 2.0 * ui.spacing().button_padding.x, h)
}

/// `bg` is the button's own fill: the info and subtitle icons are solid
/// shapes with their marks cut out in it, which is how the filled
/// versions of both read at 16px, where outlines blur into a blob.
///
/// `px` is one physical pixel in points. Minimum widths are in pixels, not
/// points: at a UI scale of 2 a 1.2pt floor is a 2.4px bar, which turned
/// the small subtitle icon into a block.
fn paint(p: &Painter, icon: Icon, rect: Rect, col: Color32, bg: Color32, px: f32) {
    let h = rect.height();
    let c = rect.center();
    match icon {
        Icon::Info => {
            let r = h * 0.38;
            p.circle_filled(c, r, col);
            let w = (h * 0.12).max(1.5 * px);
            p.circle_filled(pos2(c.x, c.y - r * 0.5), w * 0.6, bg);
            p.rect_filled(
                Rect::from_min_max(pos2(c.x - w / 2.0, c.y - r * 0.18), pos2(c.x + w / 2.0, c.y + r * 0.62)),
                0.0,
                bg,
            );
        }
        Icon::Subtitles => {
            // Bounded by the rect's width too: the chip's buttons are only
            // a little wider than tall.
            let w = (h * 0.9).min(rect.width() - 4.0 * px);
            let screen = Rect::from_center_size(c, vec2(w, h * 0.62));
            p.rect_filled(screen, h * 0.08, col);
            let (l, r) = (screen.left() + w * 0.16, screen.right() - w * 0.16);
            let gap = (h * 0.1).max(1.5 * px);
            let st = Stroke::new((h * 0.09).max(px), bg);
            for (y, split) in [(c.y - h * 0.01, 0.32), (c.y + h * 0.15, 0.62)] {
                let x = l + (r - l) * split;
                p.line_segment([pos2(l, y), pos2(x - gap / 2.0, y)], st);
                p.line_segment([pos2(x + gap / 2.0, y), pos2(r, y)], st);
            }
        }
        Icon::Audio => {
            p.text(c, egui::Align2::CENTER_CENTER, "A", egui::FontId::proportional(h * 0.72), col);
        }
        Icon::Play => {
            let th = h * 0.5;
            let tw = th * 0.87;
            // Centred on the centroid, a third of the way in from the flat
            // side; centring the bounding box looks left-heavy.
            triangle(p, pos2(c.x - tw / 3.0, c.y), tw, th, col);
        }
        Icon::Prev | Icon::Next => {
            let th = h * 0.5;
            let tw = th * 0.8;
            let bw = (h * 0.1).max(px);
            let left = c.x - (bw + tw) / 2.0;
            if icon == Icon::Prev {
                p.rect_filled(Rect::from_min_size(pos2(left, c.y - th / 2.0), vec2(bw, th)), 0.0, col);
                triangle(p, pos2(left + bw + tw, c.y), -tw, th, col);
            } else {
                triangle(p, pos2(left, c.y), tw, th, col);
                p.rect_filled(Rect::from_min_size(pos2(left + tw, c.y - th / 2.0), vec2(bw, th)), 0.0, col);
            }
        }
        Icon::Back | Icon::Forward => {
            let th = h * 0.46;
            let tw = th * 0.7;
            let dir = if icon == Icon::Back { -1.0 } else { 1.0 };
            // Two triangles nose to tail, the pair centred.
            triangle(p, pos2(c.x - dir * tw, c.y), dir * tw, th, col);
            triangle(p, pos2(c.x, c.y), dir * tw, th, col);
        }
    }
}

/// A filled triangle with its flat side vertical at `base.x`, pointing
/// right for a positive `w` and left for a negative one.
fn triangle(p: &Painter, base: Pos2, w: f32, h: f32, col: Color32) {
    let mut pts = vec![pos2(base.x, base.y - h / 2.0), pos2(base.x + w, base.y), pos2(base.x, base.y + h / 2.0)];
    // One winding for both directions: the anti-aliasing feather is laid
    // along the path's outward side, and a mirrored path feathers inward.
    if w < 0.0 {
        pts.reverse();
    }
    p.add(Shape::convex_polygon(pts, col, Stroke::NONE));
}
