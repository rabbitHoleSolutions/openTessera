//! What a client can draw over a tile's picture: a graticule, a reticle on
//! a region of the picture, and a line of readout. Built for a voice assistant's Esper
//! mode (Deckard's photo analyser), but nothing in it is Esper-specific.
//!
//! Painted on egui's background layer: above the video, under every control,
//! and never in the way of a click. It is drawn whether or not the chrome is
//! showing (that auto-hides when input goes idle).

use tessera_core::Rect;

/// Set per tile over the API (`tessera_view`); cleared with the item.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hud {
    /// A grid over the picture, with 0-100 marks along the top and left.
    pub grid: bool,
    /// A region of the picture, `(x0, y0, x1, y1)` as fractions of the whole
    /// picture (not the screen), so it stays on its subject as the view moves.
    pub reticle: Option<(f32, f32, f32, f32)>,
    /// A line of text in the corner, e.g. "ZM 4.0  ·  224 / 176".
    pub readout: String,
}

impl Hud {
    pub fn is_empty(&self) -> bool {
        !self.grid && self.reticle.is_none() && self.readout.is_empty()
    }
}

/// One tile's HUD, placed: where its picture is on screen (pixels) and which
/// part of the picture that shows.
pub struct Placed {
    pub picture: Rect,
    pub source: (f32, f32, f32, f32),
    pub hud: Hud,
}

const GREEN: egui::Color32 = egui::Color32::from_rgb(110, 255, 150);

pub fn draw(ctx: &egui::Context, huds: &[Placed]) {
    if huds.is_empty() {
        return;
    }
    let ppp = ctx.pixels_per_point();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("tessera_hud")));
    for p in huds {
        let r = egui::Rect::from_min_size(
            egui::pos2(p.picture.x / ppp, p.picture.y / ppp),
            egui::vec2(p.picture.w / ppp, p.picture.h / ppp),
        );
        if r.width() < 40.0 || r.height() < 40.0 {
            continue;
        }
        let painter = painter.with_clip_rect(r);
        let unit = r.width().min(r.height());
        let font = egui::FontId::monospace((unit / 42.0).clamp(9.0, 22.0));
        let faint = egui::Stroke::new(1.0_f32, GREEN.gamma_multiply(0.22));
        let strong = egui::Stroke::new(1.0_f32, GREEN.gamma_multiply(0.45));

        if p.hud.grid {
            // Square cells, ten across; every fifth line stronger, marks every ten units
            let step = r.width() / 10.0;
            for i in 1..10 {
                let x = r.left() + step * i as f32;
                painter.line_segment([egui::pos2(x, r.top()), egui::pos2(x, r.bottom())], if i == 5 { strong } else { faint });
                painter.text(egui::pos2(x + 3.0, r.top() + 3.0), egui::Align2::LEFT_TOP, format!("{:02}", i * 10), font.clone(), GREEN.gamma_multiply(0.6));
            }
            let rows = (r.height() / step).ceil() as i32;
            for j in 1..rows {
                let y = r.top() + step * j as f32;
                let label = (j as f32 * step / r.height() * 100.0).round() as i32;
                painter.line_segment([egui::pos2(r.left(), y), egui::pos2(r.right(), y)], if label == 50 { strong } else { faint });
                // Not where the readout sits
                if y < r.bottom() - font.size * 3.0 {
                    painter.text(egui::pos2(r.left() + 3.0, y + 2.0), egui::Align2::LEFT_TOP, format!("{label:02}"), font.clone(), GREEN.gamma_multiply(0.6));
                }
            }
            corners(&painter, r.shrink(4.0), unit * 0.06, egui::Stroke::new(2.0_f32, GREEN.gamma_multiply(0.7)));
        }

        if let Some((x0, y0, x1, y1)) = p.hud.reticle {
            // Picture fractions -> screen, through the part of the picture shown
            let (sx0, sy0, sx1, sy1) = p.source;
            let to_screen = |x: f32, y: f32| {
                egui::pos2(
                    r.left() + (x - sx0) / (sx1 - sx0).max(1e-6) * r.width(),
                    r.top() + (y - sy0) / (sy1 - sy0).max(1e-6) * r.height(),
                )
            };
            let box_ = egui::Rect::from_two_pos(to_screen(x0, y0), to_screen(x1, y1));
            if box_.intersects(r) {
                let len = (box_.width().min(box_.height()) * 0.22).clamp(6.0, unit * 0.08);
                corners(&painter, box_, len, egui::Stroke::new(2.0_f32, GREEN));
                let c = box_.center();
                let tick = len * 0.5;
                for (a, b) in [((-tick, 0.0), (tick, 0.0)), ((0.0, -tick), (0.0, tick))] {
                    painter.line_segment([c + egui::vec2(a.0, a.1), c + egui::vec2(b.0, b.1)], egui::Stroke::new(1.0_f32, GREEN.gamma_multiply(0.8)));
                }
            }
        }

        if !p.hud.readout.is_empty() {
            let pos = egui::pos2(r.left() + unit * 0.03, r.bottom() - unit * 0.03);
            let galley = painter.layout_no_wrap(p.hud.readout.clone(), font.clone(), GREEN);
            let bg = egui::Rect::from_min_size(pos - egui::vec2(6.0, galley.size().y + 4.0), galley.size() + egui::vec2(12.0, 8.0));
            painter.rect_filled(bg, 2.0, egui::Color32::from_black_alpha(170));
            painter.galley(pos - egui::vec2(0.0, galley.size().y), galley, GREEN);
        }
    }
}

/// L-shaped brackets at the four corners of `r`
fn corners(painter: &egui::Painter, r: egui::Rect, len: f32, stroke: egui::Stroke) {
    for (corner, dx, dy) in [
        (r.left_top(), 1.0, 1.0),
        (r.right_top(), -1.0, 1.0),
        (r.left_bottom(), 1.0, -1.0),
        (r.right_bottom(), -1.0, -1.0),
    ] {
        painter.line_segment([corner, corner + egui::vec2(len * dx, 0.0)], stroke);
        painter.line_segment([corner, corner + egui::vec2(0.0, len * dy)], stroke);
    }
}
