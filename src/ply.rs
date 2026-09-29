use crate::point_cloud::{Cloud, normalized_cloud};
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::Path;

const MAX_HEADER: usize = 64 * 1024;
const MAX_POINTS: u64 = 100_000_000;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Format {
    Ascii,
    BinaryLittle,
}

#[derive(Clone, Copy)]
enum Scalar {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Scalar {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "char" | "int8" => Self::I8,
            "uchar" | "uint8" => Self::U8,
            "short" | "int16" => Self::I16,
            "ushort" | "uint16" => Self::U16,
            "int" | "int32" => Self::I32,
            "uint" | "uint32" => Self::U32,
            "float" | "float32" => Self::F32,
            "double" | "float64" => Self::F64,
            _ => return None,
        })
    }
    fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }
    fn binary(self, bytes: &[u8]) -> f64 {
        match self {
            Self::I8 => i8::from_le_bytes([bytes[0]]) as f64,
            Self::U8 => bytes[0] as f64,
            Self::I16 => i16::from_le_bytes(bytes.try_into().unwrap()) as f64,
            Self::U16 => u16::from_le_bytes(bytes.try_into().unwrap()) as f64,
            Self::I32 => i32::from_le_bytes(bytes.try_into().unwrap()) as f64,
            Self::U32 => u32::from_le_bytes(bytes.try_into().unwrap()) as f64,
            Self::F32 => f32::from_le_bytes(bytes.try_into().unwrap()) as f64,
            Self::F64 => f64::from_le_bytes(bytes.try_into().unwrap()),
        }
    }
}

struct Property {
    name: String,
    scalar: Scalar,
}

struct Header {
    format: Format,
    count: u64,
    properties: Vec<Property>,
    data_start: u64,
}

fn read_header(reader: &mut BufReader<std::fs::File>) -> Result<Header, String> {
    let mut total = 0usize;
    let mut line = Vec::new();
    let mut lines = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("PLY header is truncated".into());
        }
        total = total
            .checked_add(read)
            .ok_or("PLY header length overflow")?;
        if total > MAX_HEADER {
            return Err("PLY header exceeds 64 KiB".into());
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| "PLY header is not UTF-8")?
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        let end = text == "end_header";
        lines.push(text);
        if end {
            break;
        }
    }
    if lines.first().map(String::as_str) != Some("ply") {
        return Err("Not a PLY file".into());
    }
    let mut format = None;
    let mut count = None;
    let mut properties = Vec::new();
    let mut current_vertex = false;
    let mut unsupported_elements = false;
    for line in &lines[1..] {
        let parts: Vec<_> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["format", "ascii", "1.0"] => format = Some(Format::Ascii),
            ["format", "binary_little_endian", "1.0"] => format = Some(Format::BinaryLittle),
            ["format", ..] => return Err("Unsupported PLY format".into()),
            ["element", "vertex", value] => {
                if count.is_some() {
                    return Err("PLY contains multiple vertex elements".into());
                }
                count = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "Invalid PLY vertex count")?,
                );
                current_vertex = true;
            }
            ["element", _, value] => {
                current_vertex = false;
                if value
                    .parse::<u64>()
                    .map_err(|_| "Invalid PLY element count")?
                    != 0
                {
                    unsupported_elements = true;
                }
            }
            ["property", "list", ..] if current_vertex => {
                return Err("List properties in PLY vertices are unsupported".into());
            }
            ["property", scalar, name] if current_vertex => properties.push(Property {
                name: (*name).to_owned(),
                scalar: Scalar::parse(scalar).ok_or("Unsupported PLY scalar type")?,
            }),
            ["comment", ..] | ["obj_info", ..] | ["end_header"] | [] => {}
            ["property", ..] => {}
            _ => return Err(format!("Malformed PLY header line: {line}")),
        }
    }
    if unsupported_elements {
        return Err("PLY elements after vertices are unsupported".into());
    }
    let format = format.ok_or("PLY format is missing")?;
    let count = count.ok_or("PLY vertex element is missing")?;
    if count == 0 || count > MAX_POINTS {
        return Err("Invalid or unsupported PLY vertex count".into());
    }
    for axis in ["x", "y", "z"] {
        if !properties.iter().any(|property| property.name == axis) {
            return Err(format!("PLY vertex property {axis} is missing"));
        }
    }
    Ok(Header {
        format,
        count,
        properties,
        data_start: reader.stream_position().map_err(|e| e.to_string())?,
    })
}

pub fn vertex_count(path: &Path) -> Result<u64, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(file);
    Ok(read_header(&mut reader)?.count)
}

pub fn load(path: &Path) -> Result<Cloud, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let file_len = file.metadata().map_err(|e| e.to_string())?.len();
    let mut reader = BufReader::new(file);
    let header = read_header(&mut reader)?;
    let keep = header.count.min(100_000);
    let mut retained = Vec::with_capacity(keep as usize);
    match header.format {
        Format::BinaryLittle => load_binary(&mut reader, file_len, &header, keep, &mut retained)?,
        Format::Ascii => load_ascii(&mut reader, &header, keep, &mut retained)?,
    }
    normalized_cloud(retained, header.count)
}

fn selected(index: u64, count: u64, keep: u64, retained: usize) -> bool {
    let next = retained as u64;
    next < keep && (keep == 1 || index == next * (count - 1) / (keep - 1))
}

fn fields<'a>(
    values: impl Iterator<Item = Result<(&'a str, f64), String>>,
) -> Result<([f64; 3], [u8; 3]), String> {
    let mut xyz = [None; 3];
    let mut rgb = [190u8; 3];
    for value in values {
        let (name, value) = value?;
        match name {
            "x" => xyz[0] = Some(value),
            "y" => xyz[1] = Some(value),
            "z" => xyz[2] = Some(value),
            "red" | "r" => rgb[0] = color(value)?,
            "green" | "g" => rgb[1] = color(value)?,
            "blue" | "b" => rgb[2] = color(value)?,
            _ => {}
        }
    }
    let xyz = [xyz[0].unwrap(), xyz[1].unwrap(), xyz[2].unwrap()];
    if xyz.iter().any(|value| !value.is_finite()) {
        return Err("Non-finite PLY point coordinate".into());
    }
    Ok((xyz, rgb))
}

fn color(value: f64) -> Result<u8, String> {
    if !value.is_finite() || !(0.0..=255.0).contains(&value) {
        return Err("Invalid PLY color value".into());
    }
    Ok(value.round() as u8)
}

fn load_binary(
    reader: &mut BufReader<std::fs::File>,
    file_len: u64,
    header: &Header,
    keep: u64,
    retained: &mut Vec<([f64; 3], [u8; 3])>,
) -> Result<(), String> {
    let stride: usize = header
        .properties
        .iter()
        .map(|property| property.scalar.size())
        .sum();
    let data_bytes = header
        .count
        .checked_mul(stride as u64)
        .ok_or("PLY data size overflow")?;
    let expected = header
        .data_start
        .checked_add(data_bytes)
        .ok_or("PLY file size overflow")?;
    if expected != file_len {
        return Err("PLY binary data is truncated or has trailing bytes".into());
    }
    let mut record = vec![0u8; stride];
    for index in 0..header.count {
        reader
            .read_exact(&mut record)
            .map_err(|_| "Truncated PLY vertex data")?;
        let mut offset = 0usize;
        let point = fields(header.properties.iter().map(|property| {
            let end = offset + property.scalar.size();
            let value = property.scalar.binary(&record[offset..end]);
            offset = end;
            Ok((property.name.as_str(), value))
        }))?;
        if selected(index, header.count, keep, retained.len()) {
            retained.push(point);
        }
    }
    Ok(())
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> Result<usize, String> {
    line.clear();
    loop {
        let buffer = reader.fill_buf().map_err(|e| e.to_string())?;
        if buffer.is_empty() {
            return Ok(line.len());
        }
        let take = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if line.len().saturating_add(take) > max {
            return Err("ASCII PLY vertex line is too long".into());
        }
        line.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            return Ok(line.len());
        }
    }
}

fn load_ascii(
    reader: &mut BufReader<std::fs::File>,
    header: &Header,
    keep: u64,
    retained: &mut Vec<([f64; 3], [u8; 3])>,
) -> Result<(), String> {
    let max_line = header.properties.len().saturating_mul(64).saturating_add(2);
    let mut line = Vec::with_capacity(max_line.min(4096));
    for index in 0..header.count {
        if read_bounded_line(reader, &mut line, max_line)? == 0 {
            return Err("Truncated ASCII PLY vertex data".into());
        }
        let text = std::str::from_utf8(&line).map_err(|_| "ASCII PLY vertex data is not UTF-8")?;
        let mut tokens = text.split_whitespace();
        let point = fields(header.properties.iter().map(|property| {
            let token = tokens
                .next()
                .ok_or_else(|| "ASCII PLY vertex field count is invalid".to_owned())?;
            token
                .parse::<f64>()
                .map(|value| (property.name.as_str(), value))
                .map_err(|_| "Invalid ASCII PLY scalar".to_owned())
        }))?;
        if tokens.next().is_some() {
            return Err("ASCII PLY vertex field count is invalid".into());
        }
        if selected(index, header.count, keep, retained.len()) {
            retained.push(point);
        }
    }
    loop {
        let buffer = reader.fill_buf().map_err(|e| e.to_string())?;
        if buffer.is_empty() {
            break;
        }
        if buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
            return Err("Unexpected trailing ASCII PLY data".into());
        }
        let consumed = buffer.len();
        reader.consume(consumed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    fn fixture(bytes: &[u8]) -> Result<Cloud, String> {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-scratch");
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(format!(
            "cloud-{}-{}.ply",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&p, bytes).unwrap();
        let r = load(&p);
        std::fs::remove_file(p).unwrap();
        r
    }
    fn binary_pair() -> Vec<u8> {
        let mut b = b"ply
format binary_little_endian 1.0
element vertex 2
property uchar blue
property float z
property uchar red
property float x
property uchar green
property float y
end_header
"
        .to_vec();
        for (xyz, rgb) in [
            ([10f32, 20., 30.], [1u8, 2, 255]),
            ([14., 22., 30.], [250, 0, 8]),
        ] {
            b.push(rgb[2]);
            b.extend(xyz[2].to_le_bytes());
            b.push(rgb[0]);
            b.extend(xyz[0].to_le_bytes());
            b.push(rgb[1]);
            b.extend(xyz[1].to_le_bytes());
        }
        b
    }
    #[test]
    fn parses_reordered_binary_properties_and_crlf() {
        let c = fixture(&binary_pair()).unwrap();
        assert_eq!(c.total_points, 2);
        assert_eq!(c.points[0].rgb, [1, 2, 255]);
        assert_eq!(c.points[1].rgb, [250, 0, 8]);
        assert_eq!(c.points[0].xyz, [-1., -0.5, 0.]);
        assert_eq!(c.points[1].xyz, [1., 0.5, 0.]);
    }
    #[test]
    fn parses_ascii_and_defaults_missing_color() {
        let b = b"ply
format ascii 1.0
element vertex 2
property double x
property double y
property double z
end_header
0 0 0
2 0 0
";
        let c = fixture(b).unwrap();
        assert_eq!(c.total_points, 2);
        assert_eq!(c.points[0].rgb, [190; 3]);
        assert_eq!(c.points[1].xyz, [1., 0., 0.]);
    }
    #[test]
    fn rejects_oversized_ascii_vertex_lines() {
        let mut bytes = b"ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nproperty float y\nproperty float z\nend_header\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'0', 10_000));
        bytes.push(b'\n');
        let error = fixture(&bytes).unwrap_err();
        assert!(error.contains("too long"));
    }

    #[test]
    fn rejects_malformed_truncated_nonfinite_and_lists() {
        for b in [
            b"nope".as_slice(),
            b"ply
format binary_little_endian 1.0
element vertex 1
property float x
property float y
property float z
end_header
 "
            .as_slice(),
            b"ply
format ascii 1.0
element vertex 1
property float x
property float y
property float z
end_header
NaN 0 0
"
            .as_slice(),
            b"ply
format ascii 1.0
element vertex 1
property list uchar int neighbors
property float x
property float y
property float z
end_header
0 0 0 0
"
            .as_slice(),
        ] {
            assert!(fixture(b).is_err());
        }
    }
    #[test]
    fn header_count_is_available_without_decoding_points() {
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch/count.ply");
        std::fs::create_dir_all(d.parent().unwrap()).unwrap();
        std::fs::write(&d, binary_pair()).unwrap();
        assert_eq!(vertex_count(&d).unwrap(), 2);
        std::fs::remove_file(d).unwrap();
    }
}
