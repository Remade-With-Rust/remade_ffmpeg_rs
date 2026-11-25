//! JP2 and JPX file format boxes (ISO/IEC 15444-1 Annex I).

use crate::error::{Fields, JpxError, JpxResult};
use crate::{ColorSpace, Component};

/// What the boxes around a codestream say about it.
#[derive(Default)]
pub(crate) struct Container<'a> {
    pub codestream: &'a [u8],
    pub colors: Vec<ColorSpace>,
    pub icc: Option<&'a [u8]>,
    pub palette: Option<Palette>,
    /// (component, use the palette, palette column) per channel.
    pub cmap: Vec<(u16, u8, u8)>,
    /// (channel, type, association).
    pub cdef: Vec<(u16, u16, u16)>,
}

pub(crate) struct Palette {
    pub entries: usize,
    /// (bits, signed) per column.
    pub columns: Vec<(u8, bool)>,
    /// Row by row.
    pub values: Vec<i32>,
}

/// Find the codestream, in a JP2/JPX file or on its own.
pub(crate) fn parse(data: &[u8]) -> JpxResult<Container<'_>> {
    let mut c = Container::default();
    if data.starts_with(&[0xFF, 0x4F, 0xFF, 0x51]) {
        c.codestream = data;
        return Ok(c);
    }
    let mut rest = data;
    let mut fragments = false;
    let mut header_seen = false;
    while let Some((kind, body, next)) = next_box(rest) {
        match &kind {
            b"jp2c" => {
                c.codestream = body;
                break;
            }
            b"jp2h" if !header_seen => {
                header_seen = true;
                header(body, &mut c)?;
            }
            b"ftbl" => fragments = true,
            _ => {}
        }
        rest = next;
    }
    if c.codestream.is_empty() {
        return Err(if fragments {
            JpxError::Unsupported("fragmented codestream")
        } else if data.len() < 12 {
            JpxError::Truncated
        } else {
            JpxError::Format("no JPEG 2000 codestream")
        });
    }
    Ok(c)
}

/// Split the first box off: type, contents, what follows. A box that says it
/// is longer than the data is cut there; the codestream parser reports the
/// damage.
fn next_box(data: &[u8]) -> Option<([u8; 4], &[u8], &[u8])> {
    let mut f = Fields(data);
    let len = u64::from(f.u32().ok()?);
    let kind = *f.take(4).ok()?.first_chunk::<4>()?;
    let (len, head) = match len {
        0 => (data.len() as u64, 8),
        1 => (f.uint(8).ok()?, 16),
        n => (n, 8),
    };
    if len < head {
        return None;
    }
    let end = len.min(data.len() as u64) as usize;
    let body = data.get(head as usize..end)?;
    Some((kind, body, &data[end..]))
}

fn header<'a>(mut data: &'a [u8], c: &mut Container<'a>) -> JpxResult<()> {
    while let Some((kind, body, next)) = next_box(data) {
        match &kind {
            b"colr" => color(body, c)?,
            b"pclr" if c.palette.is_none() => c.palette = Some(palette(body)?),
            b"cmap" if c.cmap.is_empty() => {
                for e in body.chunks_exact(4) {
                    c.cmap.push((u16::from_be_bytes([e[0], e[1]]), e[2], e[3]));
                }
            }
            b"cdef" if c.cdef.is_empty() => {
                let mut f = Fields(body);
                for _ in 0..f.u16()? {
                    c.cdef.push((f.u16()?, f.u16()?, f.u16()?));
                }
            }
            _ => {}
        }
        data = next;
    }
    Ok(())
}

fn color<'a>(body: &'a [u8], c: &mut Container<'a>) -> JpxResult<()> {
    let mut f = Fields(body);
    let method = f.u8()?;
    let _precedence = f.u8()?;
    let _approximation = f.u8()?;
    match method {
        1 => c.colors.push(ColorSpace::from_enum(f.u32()?)),
        // 2: restricted ICC (JP2); 3: any ICC (JPX).
        2 | 3 => {
            c.colors.push(ColorSpace::Icc);
            if c.icc.is_none() {
                c.icc = Some(f.0);
            }
        }
        _ => {}
    }
    Ok(())
}

fn palette(body: &[u8]) -> JpxResult<Palette> {
    let mut f = Fields(body);
    let entries = usize::from(f.u16()?);
    let n = usize::from(f.u8()?);
    if entries == 0 || entries > 1024 || n == 0 {
        return Err(JpxError::Format("palette size"));
    }
    let mut columns = Vec::with_capacity(n);
    for _ in 0..n {
        let b = f.u8()?;
        let bits = (b & 0x7F) + 1;
        if bits > 31 {
            return Err(JpxError::Unsupported("palette deeper than 31 bits"));
        }
        columns.push((bits, b & 0x80 != 0));
    }
    let mut values = Vec::with_capacity(entries * n);
    for _ in 0..entries {
        for &(bits, signed) in &columns {
            let v = f.uint(usize::from(bits).div_ceil(8))? as u32;
            values.push(if signed {
                let drop = 32 - u32::from(bits);
                ((v << drop) as i32) >> drop
            } else {
                (v & 0x7FFF_FFFF) as i32
            });
        }
    }
    // Some writers declare fewer bits than the entries use: believe the entries.
    for (i, (bits, signed)) in columns.iter_mut().enumerate() {
        if !*signed {
            let most = values.iter().skip(i).step_by(n).max().copied().unwrap_or(0);
            *bits = (*bits).max((32 - most.leading_zeros()) as u8);
        }
    }
    Ok(Palette {
        entries,
        columns,
        values,
    })
}

/// Replace components by the channels the `cmap` box makes of them.
pub(crate) fn apply_palette(
    components: Vec<Component>,
    palette: &Palette,
    cmap: &[(u16, u8, u8)],
    max_samples: u64,
) -> JpxResult<Vec<Component>> {
    let n = palette.columns.len();
    let mut total = 0u64;
    for &(source, kind, column) in cmap {
        let src = components
            .get(usize::from(source))
            .ok_or(JpxError::Format("component mapping"))?;
        if kind > 1 || (kind == 1 && usize::from(column) >= n) {
            return Err(JpxError::Format("component mapping"));
        }
        total += src.data.len() as u64;
    }
    if total > max_samples {
        return Err(JpxError::Limit("samples"));
    }
    let mut out = Vec::with_capacity(cmap.len());
    for &(source, kind, column) in cmap {
        let src = &components[usize::from(source)];
        let channel = if kind == 1 {
            let column = usize::from(column);
            let (bits, signed) = palette.columns[column];
            let last = palette.entries as i32 - 1;
            let entry = |&v: &i32| palette.values[v.clamp(0, last) as usize * n + column];
            Component {
                bits,
                signed,
                data: src.data.iter().map(entry).collect(),
                ..*src
            }
        } else {
            src.clone()
        };
        out.push(channel);
    }
    Ok(out)
}
