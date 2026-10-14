use qrcode::{types::QrError, Color, QrCode};

pub fn svg(data: &[u8]) -> Result<String, QrError> {
    let code = QrCode::new(data)?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .dark_color(qrcode::render::svg::Color("#0B151B"))
        .light_color(qrcode::render::svg::Color("#FFFFFF"))
        .build())
}

const SCALE: usize = 8;
const QUIET_MODULES: usize = 4;

/// Email clients such as Gmail and Outlook do not display SVG images, so ticket codes are PNG.
pub fn png(data: &[u8]) -> Result<Vec<u8>, QrError> {
    let code = QrCode::new(data)?;
    let modules = code.width();
    let colors = code.to_colors();
    let size = (modules + 2 * QUIET_MODULES) * SCALE;
    let row_bytes = size.div_ceil(8);
    let mut raw = Vec::with_capacity((row_bytes + 1) * size);
    for y in 0..size {
        raw.push(0);
        let mut row = vec![0xFF_u8; row_bytes];
        let module_y = (y / SCALE).checked_sub(QUIET_MODULES);
        for x in 0..size {
            let module_x = (x / SCALE).checked_sub(QUIET_MODULES);
            let dark = matches!((module_x, module_y), (Some(mx), Some(my))
                if mx < modules && my < modules && colors[my * modules + mx] == Color::Dark);
            if dark {
                row[x / 8] &= !(0x80 >> (x % 8));
            }
        }
        raw.extend_from_slice(&row);
    }
    let size = u32::try_from(size).map_err(|_| QrError::DataTooLong)?;
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&[1, 0, 0, 0, 0]);
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);
    Ok(png)
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = raw.chunks(0xFFFF).peekable();
    while let Some(block) = blocks.next() {
        let len = block.len() as u16;
        out.push(u8::from(blocks.peek().is_none()));
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
