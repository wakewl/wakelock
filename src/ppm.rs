// Binary PPM (P6) images, as grim writes with `-t ppm`.

/// Parses a P6 image and averages each `factor`x`factor` block into one pixel
pub fn shrink(ppm: &[u8], factor: usize) -> Option<(usize, usize, Vec<u8>)> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while fields.len() < 4 {
        while ppm.get(pos)?.is_ascii_whitespace() {
            pos += 1;
        }
        let start = pos;
        while !ppm.get(pos)?.is_ascii_whitespace() {
            pos += 1;
        }
        fields.push(std::str::from_utf8(&ppm[start..pos]).ok()?);
    }
    pos += 1;
    if fields[0] != "P6" || fields[3] != "255" {
        return None;
    }
    let width: usize = fields[1].parse().ok()?;
    let height: usize = fields[2].parse().ok()?;
    let pixels = ppm.get(pos..pos + width.checked_mul(height)?.checked_mul(3)?)?;
    if factor <= 1 {
        return Some((width, height, pixels.to_vec()));
    }

    let (out_w, out_h) = ((width / factor).max(1), (height / factor).max(1));
    let (fx, fy) = (width / out_w, height / out_h);
    let mut out = vec![0u8; out_w * out_h * 3];
    let mut sums = vec![0u32; out_w * 3];
    for oy in 0..out_h {
        sums.iter_mut().for_each(|s| *s = 0);
        for y in oy * fy..(oy + 1) * fy {
            let row = &pixels[y * width * 3..][..out_w * fx * 3];
            for (ox, block) in row.chunks_exact(fx * 3).enumerate() {
                for px in block.as_chunks::<3>().0 {
                    sums[ox * 3] += px[0] as u32;
                    sums[ox * 3 + 1] += px[1] as u32;
                    sums[ox * 3 + 2] += px[2] as u32;
                }
            }
        }
        let n = (fx * fy) as u32;
        for (o, s) in out[oy * out_w * 3..][..out_w * 3].iter_mut().zip(&sums) {
            *o = (s / n) as u8;
        }
    }
    Some((out_w, out_h, out))
}

/// Three box blurs (close to a gaussian with sigma ~ `radius`), repeating the edge pixels
pub fn blur(width: usize, height: usize, rgb: &mut [u8], radius: usize) {
    if width == 0 || height == 0 || radius == 0 {
        return;
    }
    let mut line = Vec::new();
    for _ in 0..3 {
        for y in 0..height {
            box_blur(rgb, y * width * 3, 3, width, radius, &mut line);
        }
        for x in 0..width {
            box_blur(rgb, x * 3, width * 3, height, radius, &mut line);
        }
    }
}

fn box_blur(rgb: &mut [u8], start: usize, stride: usize, len: usize, radius: usize, line: &mut Vec<[u8; 3]>) {
    line.clear();
    line.extend((0..len).map(|i| {
        let p = start + i * stride;
        [rgb[p], rgb[p + 1], rgb[p + 2]]
    }));
    let at = |i: isize| line[i.clamp(0, len as isize - 1) as usize];
    let r = radius as isize;
    let n = (2 * radius + 1) as u32;
    let mut sum = [0u32; 3];
    for i in -r..=r {
        for (s, v) in sum.iter_mut().zip(at(i)) {
            *s += v as u32;
        }
    }
    for i in 0..len as isize {
        let p = start + i as usize * stride;
        for (out, s) in rgb[p..p + 3].iter_mut().zip(sum) {
            *out = ((s + n / 2) / n) as u8;
        }
        let (add, remove) = (at(i + r + 1), at(i - r));
        for ((s, a), r) in sum.iter_mut().zip(add).zip(remove) {
            *s = *s + a as u32 - r as u32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shrink;

    #[test]
    fn averages_blocks() {
        let mut ppm = b"P6\n4 2\n255\n".to_vec();
        for row in [[0, 255, 100, 100], [255, 0, 100, 100]] {
            for v in row {
                ppm.extend_from_slice(&[v, v, v]);
            }
        }
        assert_eq!(shrink(&ppm, 2), Some((2, 1, vec![127, 127, 127, 100, 100, 100])));
        assert_eq!(shrink(&ppm, 1).map(|(w, h, d)| (w, h, d.len())), Some((4, 2, 24)));
    }

    #[test]
    fn blur_keeps_flat_images_and_spreads_edges() {
        let mut flat = vec![80u8; 5 * 4 * 3];
        super::blur(5, 4, &mut flat, 2);
        assert!(flat.iter().all(|&v| v == 80));

        let mut row = vec![0u8; 8 * 3];
        row[..3].copy_from_slice(&[255, 255, 255]);
        super::blur(8, 1, &mut row, 1);
        let reds: Vec<u8> = row.chunks(3).map(|p| p[0]).collect();
        assert!(reds[0] > reds[1] && reds[1] > reds[2] && reds[1] > 0, "{reds:?}");
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(shrink(b"P5\n1 1\n255\n\0", 1), None);
        assert_eq!(shrink(b"P6\n2 2\n255\n\0\0\0", 1), None);
        assert_eq!(shrink(b"P6\n1", 1), None);
    }
}
