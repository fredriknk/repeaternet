use std::io::{self, Write};

use crate::{Bounds, TerrainCrs};

const SIDE: usize = 256;
const BLOCK_BYTES: u64 = (SIDE * SIDE * size_of::<f32>()) as u64;
const NODATA: f32 = -32767.0;

struct Tag {
    id: u16,
    kind: u16,
    count: u64,
    bytes: Vec<u8>,
}

impl Tag {
    fn short(id: u16, value: u16) -> Self {
        Self {
            id,
            kind: 3,
            count: 1,
            bytes: value.to_le_bytes().to_vec(),
        }
    }

    fn long(id: u16, value: u32) -> Self {
        Self {
            id,
            kind: 4,
            count: 1,
            bytes: value.to_le_bytes().to_vec(),
        }
    }

    fn doubles(id: u16, values: &[f64]) -> Self {
        Self {
            id,
            kind: 12,
            count: values.len() as u64,
            bytes: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        }
    }
}

/// Write a tiled Float32 GeoTIFF on a north-up projected-metre grid.
pub fn write_elevation_tiff(
    output: &mut impl Write,
    crs: TerrainCrs,
    bounds: Bounds,
    width: u32,
    height: u32,
    values: &[f32],
) -> io::Result<()> {
    let code = crs.epsg_code().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Download target requires a registered CRS",
        )
    })?;
    if width == 0
        || height == 0
        || u64::from(width) * u64::from(height) != values.len() as u64
        || ![bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y]
            .into_iter()
            .all(f64::is_finite)
        || bounds.min_x >= bounds.max_x
        || bounds.min_y >= bounds.max_y
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid GeoTIFF output grid",
        ));
    }
    let (width_usize, height_usize) = (width as usize, height as usize);
    let (cols, rows) = (width_usize.div_ceil(SIDE), height_usize.div_ceil(SIDE));
    let blocks = (cols * rows) as u64;
    let big = blocks.saturating_mul(BLOCK_BYTES + 8).saturating_add(4096) > u64::from(u32::MAX);
    let offset_len = if big { 8 } else { 4 };
    let offset_kind = if big { 16 } else { 4 };
    let pack = |values: &mut dyn Iterator<Item = u64>| -> Vec<u8> {
        values
            .flat_map(|v| v.to_le_bytes().into_iter().take(offset_len))
            .collect()
    };
    let keys: [u16; 16] = [1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, code];
    let scale = [
        (bounds.max_x - bounds.min_x) / f64::from(width),
        (bounds.max_y - bounds.min_y) / f64::from(height),
        0.0,
    ];
    let tags = [
        Tag::long(256, width),
        Tag::long(257, height),
        Tag::short(258, 32),
        Tag::short(259, 1),
        Tag::short(262, 1),
        Tag::short(277, 1),
        Tag::long(322, SIDE as u32),
        Tag::long(323, SIDE as u32),
        Tag {
            id: 324,
            kind: offset_kind,
            count: blocks,
            bytes: pack(&mut (0..blocks).map(|_| 0)),
        },
        Tag {
            id: 325,
            kind: offset_kind,
            count: blocks,
            bytes: pack(&mut (0..blocks).map(|_| BLOCK_BYTES)),
        },
        Tag::short(339, 3),
        Tag::doubles(33550, &scale),
        Tag::doubles(33922, &[0.0, 0.0, 0.0, bounds.min_x, bounds.max_y, 0.0]),
        Tag {
            id: 34735,
            kind: 3,
            count: keys.len() as u64,
            bytes: keys.iter().flat_map(|v| v.to_le_bytes()).collect(),
        },
        Tag {
            id: 42113,
            kind: 2,
            count: 7,
            bytes: b"-32767\0".to_vec(),
        },
    ];
    let mut directory = if big {
        b"II+\0\x08\0\0\0\x10\0\0\0\0\0\0\0".to_vec()
    } else {
        b"II*\0\x08\0\0\0".to_vec()
    };
    let count_len = if big { 8 } else { 2 };
    let mut next = directory.len() + count_len + tags.len() * (4 + 2 * offset_len) + offset_len;
    let start = next
        + tags
            .iter()
            .map(|tag| {
                if tag.bytes.len() > offset_len {
                    tag.bytes.len()
                } else {
                    0
                }
            })
            .sum::<usize>();
    directory.extend_from_slice(&(tags.len() as u64).to_le_bytes()[..count_len]);
    let mut extra = Vec::new();
    for mut tag in tags {
        if tag.id == 324 {
            tag.bytes = pack(&mut (0..blocks).map(|i| start as u64 + i * BLOCK_BYTES));
        }
        directory.extend(tag.id.to_le_bytes());
        directory.extend(tag.kind.to_le_bytes());
        directory.extend_from_slice(&tag.count.to_le_bytes()[..offset_len]);
        if tag.bytes.len() > offset_len {
            directory.extend_from_slice(&(next as u64).to_le_bytes()[..offset_len]);
            next += tag.bytes.len();
            extra.extend(tag.bytes);
        } else {
            tag.bytes.resize(offset_len, 0);
            directory.extend(tag.bytes);
        }
    }
    directory.extend_from_slice(&[0; 8][..offset_len]);
    output.write_all(&directory)?;
    output.write_all(&extra)?;
    let mut block = vec![0; BLOCK_BYTES as usize];
    for tile_row in 0..rows {
        for tile_col in 0..cols {
            for (index, bytes) in block.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let row = tile_row * SIDE + index / SIDE;
                let col = tile_col * SIDE + index % SIDE;
                let value = if row < height_usize && col < width_usize {
                    values[row * width_usize + col]
                } else {
                    NODATA
                };
                bytes
                    .copy_from_slice(&if value.is_finite() { value } else { NODATA }.to_le_bytes());
            }
            output.write_all(&block)?;
        }
    }
    Ok(())
}
