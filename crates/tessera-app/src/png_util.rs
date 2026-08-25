use std::io::BufWriter;
use std::path::Path;

/// Writes an RGB8 buffer (as produced by glReadPixels, bottom row first) as a
/// PNG with top row first.
pub fn write_png_rgb(path: &Path, w: i32, h: i32, bottom_up_rgb: &[u8]) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let stride = (w as usize) * 3;
    let mut flipped = vec![0u8; bottom_up_rgb.len()];
    for row in 0..h as usize {
        let src = &bottom_up_rgb[row * stride..(row + 1) * stride];
        let dst = &mut flipped[(h as usize - 1 - row) * stride..(h as usize - row) * stride];
        dst.copy_from_slice(src);
    }
    writer
        .write_image_data(&flipped)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(())
}
