/// Rectangle in pixels with origin at the top-left of the screen, y growing down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
}

/// Computes cell rects for `count` tiles on a screen, choosing the rows/cols
/// grid that maximizes coverage for `aspect` (w/h) tiles, ported in spirit
/// from the Quest app's LayoutSolver coverage scoring.
pub fn solve_grid(count: usize, screen_w: f32, screen_h: f32, aspect: f32, gap: f32) -> Vec<Rect> {
    if count == 0 || screen_w <= 0.0 || screen_h <= 0.0 {
        return Vec::new();
    }
    let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 16.0 / 9.0 };

    let mut best: Option<(f32, usize, usize)> = None;
    for rows in 1..=count {
        for cols in 1..=count {
            if rows * cols < count {
                continue;
            }
            let avail_w = screen_w - gap * (cols as f32 + 1.0);
            let avail_h = screen_h - gap * (rows as f32 + 1.0);
            if avail_w <= 0.0 || avail_h <= 0.0 {
                continue;
            }
            let cell_w = avail_w / cols as f32;
            let cell_h = avail_h / rows as f32;
            let tile_w = cell_w.min(cell_h * aspect);
            let tile_h = tile_w / aspect;
            let used = tile_w * tile_h * count as f32;
            let empty = (rows * cols - count) as f32;
            let penalty = empty * tile_w * tile_h * 0.75;
            let score = used - penalty;
            if best.map_or(true, |(s, _, _)| score > s) {
                best = Some((score, rows, cols));
            }
        }
    }
    // No candidate had room for even one cell once the gaps were taken out
    // — a window a few tens of pixels across, which a WM can produce mid
    // -resize. An empty layout draws no tiles for a frame; the `expect`
    // that used to be here took the whole wall down instead.
    let Some((_, rows, cols)) = best else {
        return Vec::new();
    };

    let avail_w = screen_w - gap * (cols as f32 + 1.0);
    let avail_h = screen_h - gap * (rows as f32 + 1.0);
    let cell_w = avail_w / cols as f32;
    let cell_h = avail_h / rows as f32;

    let mut rects = Vec::with_capacity(count);
    for i in 0..count {
        let col = i % cols;
        let row = i / cols;
        let cell_x = gap + col as f32 * (cell_w + gap);
        let cell_y = gap + row as f32 * (cell_h + gap);
        rects.push(Rect { x: cell_x, y: cell_y, w: cell_w, h: cell_h });
    }
    rects
}

/// Fits a rect of `video_aspect` (w/h) inside `cell`, letterboxed and centered.
pub fn fit_aspect(cell: &Rect, video_aspect: f32) -> Rect {
    let aspect = if video_aspect.is_finite() && video_aspect > 0.0 { video_aspect } else { 16.0 / 9.0 };
    let w = cell.w.min(cell.h * aspect);
    let h = w / aspect;
    Rect {
        x: cell.x + (cell.w - w) * 0.5,
        y: cell.y + (cell.h - h) * 0.5,
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 1920.0;
    const H: f32 = 1080.0;
    const A: f32 = 16.0 / 9.0;

    #[test]
    fn single_tile_fills_screen() {
        let rects = solve_grid(1, W, H, A, 8.0);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].w - (W - 16.0)).abs() < 0.01);
        assert!((rects[0].h - (H - 16.0)).abs() < 0.01);
    }

    #[test]
    fn four_tiles_form_quad() {
        let rects = solve_grid(4, W, H, A, 8.0);
        assert_eq!(rects.len(), 4);
        assert!((rects[0].y - rects[1].y).abs() < 0.01);
        assert!(rects[2].y > rects[0].y);
        assert!(rects[1].x > rects[0].x);
    }

    #[test]
    fn five_tiles_prefer_3x2_over_5x1() {
        let rects = solve_grid(5, W, H, A, 8.0);
        // tiles must span more than one row
        let ys: Vec<f32> = rects.iter().map(|r| r.y).collect();
        let distinct_rows = ys.iter().enumerate().filter(|(i, y)| ys[..*i].iter().all(|y2| (y2 - **y).abs() > 1.0)).count();
        assert!(distinct_rows >= 2, "expected multi-row layout, ys={ys:?}");
    }

    #[test]
    fn cells_cover_screen_without_overlap() {
        let rects = solve_grid(7, 3440.0, 1440.0, A, 10.0);
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                let overlap = a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
                assert!(!overlap, "cells {a:?} and {b:?} overlap");
            }
            assert!(a.x >= 0.0 && a.y >= 0.0 && a.x + a.w <= 3440.0 && a.y + a.h <= 1440.0);
        }
    }

    #[test]
    fn fit_aspect_letterboxes() {
        let cell = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 1000.0 };
        let fit = fit_aspect(&cell, 16.0 / 9.0);
        assert!((fit.w - 1000.0).abs() < 0.01);
        assert!((fit.h - 562.5).abs() < 0.01);
        assert!((fit.y - 218.75).abs() < 0.01);
    }

    #[test]
    fn portrait_screen() {
        let rects = solve_grid(3, 2160.0, 3840.0, A, 8.0);
        assert_eq!(rects.len(), 3);
        for r in &rects {
            assert!(r.w > 0.0 && r.h > 0.0);
        }
    }

    #[test]
    fn a_window_too_small_for_any_grid_yields_no_cells() {
        // 12 tiles at gap 8 needs at least 3 columns and 4 rows, which is
        // 32px of gap in each direction before a single cell exists.
        assert!(solve_grid(12, 40.0, 40.0, A, 8.0).is_empty());
        // Still zero, not a panic, at the degenerate end.
        assert!(solve_grid(16, 1.0, 1.0, A, 8.0).is_empty());
        // And a window with room is unaffected.
        assert_eq!(solve_grid(12, W, H, A, 8.0).len(), 12);
    }

    #[test]
    fn contains_hit_testing() {
        let r = Rect { x: 10.0, y: 20.0, w: 100.0, h: 50.0 };
        assert!(r.contains(15.0, 25.0));
        assert!(!r.contains(5.0, 25.0));
        assert!(!r.contains(115.0, 25.0));
    }
}
