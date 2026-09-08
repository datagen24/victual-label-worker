//! Reading `raster/png-indexed;v=1` back into ink planes.
//!
//! The worker decodes what it was handed rather than trusting the manifest's account of it,
//! for the same reason Victual verifies rather than trusting the renderer's: a manifest field
//! that merely repeats a claim records an intention. What the device is given has to come from
//! the bytes.

pub struct Raster {
    pub width: u32,
    pub height: u32,
    /// Palette index per pixel, row-major.
    pub indices: Vec<u8>,
    pub palette: Vec<[u8; 3]>,
}

impl Raster {
    /// True where this colour is inked.
    pub fn plane(&self, colour: [u8; 3]) -> Option<Vec<bool>> {
        let index = self.palette.iter().position(|c| *c == colour)? as u8;
        Some(self.indices.iter().map(|i| *i == index).collect())
    }

    pub fn has(&self, colour: [u8; 3]) -> bool {
        self.plane(colour).map(|p| p.iter().any(|b| *b)).unwrap_or(false)
    }
}

pub fn decode(bytes: &[u8]) -> Result<Raster, String> {
    if bytes.len() < 8 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return Err("the artifact is not a PNG".into());
    }

    let mut offset = 8usize;
    let mut header: Option<(u32, u32, u8, u8)> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();

    while offset + 8 <= bytes.len() {
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let kind = &bytes[offset + 4..offset + 8];
        if offset + 12 + length > bytes.len() {
            return Err(format!("truncated {} chunk", String::from_utf8_lossy(kind)));
        }
        let data = &bytes[offset + 8..offset + 8 + length];
        match kind {
            b"IHDR" => header = Some((
                u32::from_be_bytes(data[0..4].try_into().unwrap()),
                u32::from_be_bytes(data[4..8].try_into().unwrap()),
                data[8], data[9])),
            b"PLTE" => {
                if length % 3 != 0 {
                    return Err("the palette length is not a multiple of three".into());
                }
                palette = data.chunks(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        offset += 12 + length;
    }

    let (width, height, depth, colour_type) = header.ok_or("the artifact has no IHDR chunk")?;
    if colour_type != 3 || depth != 2 {
        // The form identifier pins both. Quantising here would be the worker deciding what
        // the label looks like, which is the thing the artifact contract exists to prevent.
        return Err(format!("the artifact is colour type {colour_type} at bit depth {depth}; raster/png-indexed;v=1 is colour type 3 at bit depth 2"));
    }
    if palette.is_empty() {
        return Err("the artifact carries no palette".into());
    }

    let raw = inflate(&idat)?;
    let per_byte = 4usize;
    let stride = (width as usize).div_ceil(per_byte);
    let mut indices = Vec::with_capacity((width * height) as usize);
    let mut previous = vec![0u8; stride];
    let mut position = 0usize;

    for _ in 0..height {
        if position >= raw.len() {
            return Err("fewer scanlines than the header declares".into());
        }
        let filter = raw[position];
        position += 1;
        if position + stride > raw.len() {
            return Err("a truncated scanline".into());
        }
        let mut line = raw[position..position + stride].to_vec();
        position += stride;

        for i in 0..stride {
            let a = if i > 0 { line[i - 1] } else { 0 };
            let b = previous[i];
            let c = if i > 0 { previous[i - 1] } else { 0 };
            line[i] = match filter {
                0 => line[i],
                1 => line[i].wrapping_add(a),
                2 => line[i].wrapping_add(b),
                3 => line[i].wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => line[i].wrapping_add(paeth(a, b, c)),
                other => return Err(format!("unknown PNG filter type {other}")),
            };
        }
        previous = line.clone();

        for x in 0..width as usize {
            let byte = line[x / per_byte];
            let shift = ((per_byte - 1) - (x % per_byte)) * 2;
            indices.push((byte >> shift) & 0b11);
        }
    }

    Ok(Raster { width, height, indices, palette })
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
}

fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| format!("the artifact image data does not inflate: {e}"))?;
    Ok(out)
}
