//! A picture as a few average colours: what lights that follow the video
//! (a voice assistant's ambilight) are given, many times a second. The GPU
//! has already shrunk the frame to a few texels a cell; this averages each
//! cell of a `w`×`h` grid over them.

/// The average colour of each cell of a `w`×`h` grid laid over an RGB8
/// picture `sw`×`sh` (rows in the same order in and out). Every cell gets at
/// least one pixel, so a picture smaller than the grid repeats pixels rather
/// than leaving cells black.
pub fn box_average(px: &[u8], sw: usize, sh: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    if sw == 0 || sh == 0 || px.len() < sw * sh * 3 {
        return out;
    }
    let span = |i: usize, n: usize, size: usize| {
        let a = (i * size / n).min(size - 1);
        (a, ((i + 1) * size / n).clamp(a + 1, size))
    };
    for gy in 0..h {
        let (y0, y1) = span(gy, h, sh);
        for gx in 0..w {
            let (x0, x1) = span(gx, w, sw);
            let mut sum = [0u32; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = (y * sw + x) * 3;
                    for k in 0..3 {
                        sum[k] += px[p + k] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (gy * w + gx) * 3;
            for k in 0..3 {
                out[o + k] = ((sum[k] + n / 2) / n) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_cell_is_the_average_of_its_pixels() {
        // 4x2: a red half and a blue half, one pixel of the red half white
        let mut px = Vec::new();
        for c in [[255, 0, 0], [255, 0, 0], [0, 0, 255], [0, 0, 255], [255, 255, 255], [255, 0, 0], [0, 0, 255], [0, 0, 255]] {
            px.extend_from_slice(&c);
        }
        assert_eq!(box_average(&px, 4, 2, 2, 1), vec![255, 64, 64, 0, 0, 255]);
        // The whole picture as one colour
        assert_eq!(box_average(&px, 4, 2, 1, 1), vec![128, 32, 159]);
    }

    #[test]
    fn a_picture_smaller_than_the_grid_fills_every_cell() {
        let px = vec![10, 20, 30, 200, 100, 0];
        assert_eq!(box_average(&px, 2, 1, 4, 2), vec![10, 20, 30, 10, 20, 30, 200, 100, 0, 200, 100, 0, 10, 20, 30, 10, 20, 30, 200, 100, 0, 200, 100, 0]);
        // Nothing to read: black, not a panic
        assert_eq!(box_average(&[], 0, 0, 2, 1), vec![0; 6]);
    }
}
