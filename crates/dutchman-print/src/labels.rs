//! TSPL facts: TSC TSPL/TSPL2 Programming Manual, SIZE, GAP, CLS, BITMAP,
//! PRINT, <ESC>!? and <ESC>!. See the local protocol notes for commissioning limits.
use crate::backend::{Error, Printer, PrinterState};
use crate::config;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

pub const MAX_PNG: usize = 16 << 20;
pub const TIMEOUT: Duration = Duration::from_secs(2);
fn refused(text: &str) -> Error {
    Error::Refused(text.into())
}
pub fn validate(p: &config::Printer) -> Result<(), String> {
    if p.language.as_deref() != Some("tspl") {
        return Err("Set label language to tspl.".into());
    }
    if p.host.as_ref().is_none_or(|s| {
        s.is_empty()
            || s.len() > 253
            || s.chars().any(|c| c.is_whitespace() || c.is_control())
            || s.contains(['/', '\\', '@'])
    }) || p.port == Some(0)
    {
        return Err("Set a label host and a nonzero port (default 9100).".into());
    }
    if !matches!(p.dpi, Some(203 | 300)) {
        return Err("Set label dpi explicitly to 203 or 300.".into());
    }
    for value in [p.width_mm, p.height_mm] {
        if value.is_none_or(|v| !v.is_finite() || v <= 0.0 || v > 1000.0) {
            return Err("Set label width_mm and height_mm above zero and at most 1000 mm.".into());
        }
    }
    if p.gap_mm
        .is_none_or(|v| !v.is_finite() || !(0.0..=25.4).contains(&v))
    {
        return Err("Set label gap_mm between 0 and 25.4 mm.".into());
    }
    if p.slicer.is_some()
        || p.printer_profile.is_some()
        || p.print_profile.is_some()
        || p.filament_profile.is_some()
    {
        return Err("Label printers use PNG directly. Leave out slicers and profiles.".into());
    }
    Ok(())
}
pub fn map_status(code: u8) -> (PrinterState, String) {
    let mut reasons = vec![];
    for (bit, text) in [
        (1, "Head open"),
        (2, "Paper jam"),
        (4, "Paper empty"),
        (8, "Ribbon empty"),
    ] {
        if code & bit != 0 {
            reasons.push(text);
        }
    }
    let state = if code & 0xc0 != 0 {
        reasons.push("Other error");
        PrinterState::Error
    } else if !reasons.is_empty() {
        PrinterState::Attention
    } else if code & 0x10 != 0 {
        reasons.push("Paused");
        PrinterState::Paused
    } else if code == 0x20 {
        reasons.push("Printing");
        PrinterState::Printing
    } else if code == 0 {
        reasons.push("Ready");
        PrinterState::Idle
    } else {
        reasons.push("Unknown status");
        PrinterState::Error
    };
    (state, reasons.join("; "))
}
pub struct Client {
    config: config::Printer,
}
impl Client {
    pub fn new(config: config::Printer) -> Result<Self, String> {
        validate(&config)?;
        Ok(Self { config })
    }
    pub fn config(&self) -> &config::Printer {
        &self.config
    }
    pub async fn connect(&self) -> Result<TcpStream, Error> {
        let host = self.config.host.as_deref().ok_or(Error::Unavailable)?;
        tokio::time::timeout(
            TIMEOUT,
            TcpStream::connect((host, self.config.port.unwrap_or(9100))),
        )
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|_| Error::Unavailable)
    }
    pub async fn status_on(stream: &mut TcpStream) -> Result<u8, Error> {
        tokio::time::timeout(TIMEOUT, async {
            stream.write_all(b"\x1b!?").await?;
            stream.read_u8().await
        })
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(|_| Error::Unavailable)
    }
    pub async fn status(&self) -> Result<u8, Error> {
        Self::status_on(&mut self.connect().await?).await
    }
    pub async fn snapshot(&self) -> Printer {
        let (state, detail) = match self.status().await {
            Ok(code) => map_status(code),
            Err(_) => (
                PrinterState::Offline,
                "Fresh TSPL status unavailable. Check the printer connection.".into(),
            ),
        };
        Printer {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            model: self.config.model.clone(),
            state,
            detail: Some(detail),
            controls: vec!["start".into(), "cancel".into()],
            needs_clearing: Some(false),
            job_path: None,
            job_id: None,
            job_name: None,
            progress: None,
            seconds_left: None,
            nozzle_temperature: None,
            bed_temperature: None,
        }
    }
}
pub fn ready(code: u8) -> Result<(), Error> {
    if code != 0 {
        return Err(refused("The label printer is not ready. Read status and resolve active work or attention before starting."));
    }
    Ok(())
}
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}
fn invalid() -> Error {
    refused("Cannot decode PNG. Use a valid noninterlaced PNG with 8-bit grayscale, RGB, grayscale-alpha, or RGBA pixels.")
}
fn number(b: &[u8]) -> Result<u32, Error> {
    Ok(u32::from_be_bytes(b.try_into().map_err(|_| invalid())?))
}
fn crc(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for byte in b {
        c ^= u32::from(*byte);
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb88320u32.wrapping_mul(c & 1));
        }
    }
    !c
}
/// The PNG container and scanline filters are implemented here; zlib inflation uses
/// the MIT/Apache flate2 crate already present in Cargo.lock. No image renderer is used.
pub fn decode(bytes: &[u8]) -> Result<Raster, Error> {
    use std::io::Read as _;
    if bytes.len() > MAX_PNG {
        return Err(refused("PNG exceeds 16 MiB. Choose a smaller label image."));
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(refused(
            "Label printers require PNG images. PDF and 3MF jobs cannot be printed here.",
        ));
    }
    let mut remaining = bytes.get(8..).ok_or_else(invalid)?;
    let (mut width, mut height, mut channels) = (0usize, 0usize, 0usize);
    let mut compressed = Vec::new();
    let mut ended = false;
    let mut chunks = 0;
    let mut idat = false;
    let mut after_idat = false;
    while !remaining.is_empty() {
        chunks += 1;
        if chunks > 4096 {
            return Err(invalid());
        }
        let len = number(remaining.get(..4).ok_or_else(invalid)?)? as usize;
        let chunk = remaining
            .get(4..len.checked_add(8).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        let tag = chunk.get(..4).ok_or_else(invalid)?;
        let data = chunk.get(4..).ok_or_else(invalid)?;
        let check = number(remaining.get(len + 8..len + 12).ok_or_else(invalid)?)?;
        if crc(chunk) != check {
            return Err(invalid());
        }
        remaining = remaining.get(len + 12..).ok_or_else(invalid)?;
        if chunks == 1 && tag != b"IHDR" {
            return Err(invalid());
        }
        match tag {
            b"IHDR" => {
                if chunks != 1 || len != 13 {
                    return Err(invalid());
                }
                width = number(data.get(..4).ok_or_else(invalid)?)? as usize;
                height = number(data.get(4..8).ok_or_else(invalid)?)? as usize;
                if width == 0
                    || height == 0
                    || width > 12000
                    || height > 12000
                    || width.checked_mul(height).is_none_or(|n| n > 4_000_000)
                {
                    return Err(refused("PNG dimensions exceed 12000 dots or 4 million pixels. Choose a smaller label image."));
                }
                if data.get(8) != Some(&8) || data.get(10..) != Some(&[0, 0, 0][..]) {
                    return Err(invalid());
                }
                channels = match data.get(9) {
                    Some(0) => 1,
                    Some(2) => 3,
                    Some(4) => 2,
                    Some(6) => 4,
                    _ => return Err(invalid()),
                };
            }
            b"IDAT" => {
                if after_idat {
                    return Err(invalid());
                }
                idat = true;
                compressed.extend_from_slice(data);
            }
            b"IEND" => {
                if len != 0 || !idat || !remaining.is_empty() {
                    return Err(invalid());
                }
                ended = true;
                break;
            }
            // Palette is legal but unused for the supported truecolor/grayscale modes.
            b"PLTE" if !idat && channels >= 3 && len > 0 && len <= 768 && len.is_multiple_of(3) => {
            }
            // Transparency keys would alter the raster; require explicit alpha instead.
            b"tRNS" | b"acTL" | b"fcTL" | b"fdAT" => return Err(invalid()),
            _ => {
                if tag.first().is_none_or(|b| b & 32 == 0) {
                    return Err(invalid());
                }
                if idat {
                    after_idat = true;
                }
            }
        }
    }
    if !ended {
        return Err(invalid());
    }
    let stride = width.checked_mul(channels).ok_or_else(invalid)?;
    let expected = (stride + 1).checked_mul(height).ok_or_else(invalid)?;
    let mut decoder = flate2::read::ZlibDecoder::new(compressed.as_slice());
    let mut raw = Vec::new();
    decoder
        .by_ref()
        .take(expected as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|_| invalid())?;
    if raw.len() != expected || decoder.total_in() != compressed.len() as u64 {
        return Err(invalid());
    }
    let row_bytes = width.div_ceil(8);
    // BITMAP's example uses zero for ink and one for white, MSB first.
    let mut raster = vec![255; row_bytes * height];
    let mut previous = vec![0u8; stride];
    for (y, line) in raw.chunks_exact(stride + 1).enumerate() {
        let filter = *line.first().ok_or_else(invalid)?;
        let mut row = line.get(1..).ok_or_else(invalid)?.to_vec();
        for i in 0..stride {
            let a = if i >= channels {
                *row.get(i - channels).ok_or_else(invalid)?
            } else {
                0
            };
            let b = *previous.get(i).ok_or_else(invalid)?;
            let c = if i >= channels {
                *previous.get(i - channels).ok_or_else(invalid)?
            } else {
                0
            };
            let predictor = match filter {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                4 => {
                    let p = i32::from(a) + i32::from(b) - i32::from(c);
                    let pa = (p - i32::from(a)).abs();
                    let pb = (p - i32::from(b)).abs();
                    let pc = (p - i32::from(c)).abs();
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => return Err(invalid()),
            };
            let v = row.get_mut(i).ok_or_else(invalid)?;
            *v = v.wrapping_add(predictor);
        }
        for (x, pixel) in row.chunks_exact(channels).enumerate() {
            let r = u32::from(*pixel.first().ok_or_else(invalid)?);
            let luma = if channels >= 3 {
                (299 * r
                    + 587 * u32::from(*pixel.get(1).ok_or_else(invalid)?)
                    + 114 * u32::from(*pixel.get(2).ok_or_else(invalid)?))
                    / 1000
            } else {
                r
            };
            let alpha = if channels == 2 || channels == 4 {
                u32::from(*pixel.last().ok_or_else(invalid)?)
            } else {
                255
            };
            let white = (luma * alpha + 255 * (255 - alpha)) / 255;
            if white < 128 {
                let byte = raster.get_mut(y * row_bytes + x / 8).ok_or_else(invalid)?;
                *byte &= !(0x80 >> (x % 8));
            }
        }
        previous = row;
    }
    Ok(Raster {
        width: width as u32,
        height: height as u32,
        bytes: raster,
    })
}
pub fn job(p: &config::Printer, bytes: &[u8], copies: u32) -> Result<Vec<u8>, Error> {
    let raster = decode(bytes)?;
    let dpi = p.dpi.ok_or(Error::Unavailable)?;
    let width = p.width_mm.ok_or(Error::Unavailable)?;
    let height = p.height_mm.ok_or(Error::Unavailable)?;
    let expected_w = (width * f64::from(dpi) / 25.4).round() as u32;
    let expected_h = (height * f64::from(dpi) / 25.4).round() as u32;
    if raster.width.abs_diff(expected_w) > 1 || raster.height.abs_diff(expected_h) > 1 {
        return Err(Error::Refused(format!("PNG is {} × {} dots; label stock is {} × {} dots ({} × {} mm at {} dpi). Export a matching PNG; scaling is unavailable.",raster.width,raster.height,expected_w,expected_h,width,height,dpi)));
    }
    if !(1..=999_999_999).contains(&copies) {
        return Err(refused("Use 1–999999999 label copies."));
    }
    let mut output = format!(
        "SIZE {width} mm,{height} mm\r\nGAP {} mm,0 mm\r\nCLS\r\nBITMAP 0,0,{},{},0,",
        p.gap_mm.ok_or(Error::Unavailable)?,
        raster.width.div_ceil(8),
        raster.height
    )
    .into_bytes();
    output.extend(raster.bytes);
    output.extend(format!("\r\nPRINT 1,{copies}\r\n").bytes());
    Ok(output)
}
