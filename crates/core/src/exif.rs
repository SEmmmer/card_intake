//! A small EXIF reader for the fields Card Intake needs: camera make, model, serial
//! number, firmware, lens and capture time.
//!
//! Reads TIFF-based raws (ARW, DNG, Nikon's NEF/NRW, Hasselblad's 3FR), JPEGs and HEIF
//! (Sony's HIF, HEIC). It seeks straight to the few tags it needs, so it reads a few KB
//! per file instead of the whole 60 MB raw.

use chrono::NaiveDateTime;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BasicExif {
    pub make: Option<String>,
    pub model: Option<String>,
    /// Firmware version, e.g. "ILCE-7RM6 v1.00".
    pub software: Option<String>,
    pub serial: Option<String>,
    pub lens: Option<String>,
    /// Camera clock time when the shutter fired (`DateTimeOriginal`).
    pub captured: Option<NaiveDateTime>,
}

const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_SOFTWARE: u16 = 0x0131;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_DATE_TIME_ORIGINAL: u16 = 0x9003;
const TAG_BODY_SERIAL_NUMBER: u16 = 0xA431;
const TAG_LENS_MODEL: u16 = 0xA434;
/// DNG's serial tag, in IFD0. Hasselblad's 3FRs and JPEGs carry the serial only here.
const TAG_CAMERA_SERIAL_NUMBER: u16 = 0xC62F;

const TYPE_ASCII: u16 = 2;
const MAX_ENTRIES: u16 = 1024;
const MAX_STRING: u32 = 512;

pub fn read_basic(path: &Path) -> io::Result<BasicExif> {
    read_basic_from(&mut BufReader::new(File::open(path)?))
}

pub fn read_basic_from<R: Read + Seek>(r: &mut R) -> io::Result<BasicExif> {
    let mut magic = [0u8; 8];
    r.seek(SeekFrom::Start(0))?;
    r.read_exact(&mut magic)?;
    let base = match magic {
        [b'I', b'I', ..] | [b'M', b'M', ..] => 0,
        [0xFF, 0xD8, ..] => jpeg_tiff_offset(r)?,
        [_, _, _, _, b'f', b't', b'y', b'p'] => heif_tiff_offset(r)?,
        _ => return Err(invalid("not a TIFF-based raw, a JPEG or a HEIF photo")),
    };

    let mut tiff = Tiff::open(r, base)?;
    let ifd0 = tiff.entries(tiff.first_ifd)?;
    let mut out = BasicExif {
        make: tiff.ascii(&ifd0, TAG_MAKE)?,
        model: tiff.ascii(&ifd0, TAG_MODEL)?,
        software: tiff.ascii(&ifd0, TAG_SOFTWARE)?,
        ..BasicExif::default()
    };
    if let Some(offset) = tiff.offset(&ifd0, TAG_EXIF_IFD) {
        let exif = tiff.entries(offset)?;
        out.captured = tiff
            .ascii(&exif, TAG_DATE_TIME_ORIGINAL)?
            .and_then(|s| NaiveDateTime::parse_from_str(&s, "%Y:%m:%d %H:%M:%S").ok());
        out.serial = tiff.ascii(&exif, TAG_BODY_SERIAL_NUMBER)?;
        out.lens = tiff.ascii(&exif, TAG_LENS_MODEL)?;
    }
    if out.serial.is_none() {
        out.serial = tiff.ascii(&ifd0, TAG_CAMERA_SERIAL_NUMBER)?;
    }
    Ok(out)
}

/// Whether a photo starts the way its extension says: a TIFF header for raws, a JPEG
/// marker, or a HEIF `ftyp` box. Other extensions aren't judged.
///
/// When Windows shows one card's folders over another card's data, the names are
/// right and the bytes aren't; this tells without reading the whole photo.
pub fn content_matches(path: &Path) -> io::Result<bool> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut head = [0u8; 8];
    let read = File::open(path)?.read(&mut head)?;
    let head = &head[..read];
    Ok(match ext.as_str() {
        "arw" | "nef" | "nrw" | "3fr" | "dng" => {
            head.starts_with(b"II*\0") || head.starts_with(b"MM\0*")
        }
        "jpg" | "jpeg" => head.starts_with(&[0xFF, 0xD8, 0xFF]),
        "hif" | "heic" | "heif" => head.get(4..8) == Some(b"ftyp".as_slice()),
        _ => true,
    })
}

/// The largest `iinf` or `iloc` box read: a camera's are a few hundred bytes.
const MAX_HEIF_BOX: u64 = 1 << 20;

/// The payload of the first `kind` box between `from` and `to`: its start and length.
fn find_box<R: Read + Seek>(
    r: &mut R,
    from: u64,
    to: u64,
    kind: &[u8; 4],
) -> io::Result<Option<(u64, u64)>> {
    let mut pos = from;
    while pos + 8 <= to {
        let mut head = [0u8; 16];
        r.seek(SeekFrom::Start(pos))?;
        r.read_exact(&mut head[..8])?;
        let mut size = u32::from_be_bytes(head[..4].try_into().unwrap()) as u64;
        let mut header = 8;
        if size == 1 {
            r.read_exact(&mut head[8..])?;
            size = u64::from_be_bytes(head[8..].try_into().unwrap());
            header = 16;
        } else if size == 0 {
            size = to - pos;
        }
        if size < header || size > to - pos {
            return Err(invalid("broken HEIF box"));
        }
        if &head[4..8] == kind {
            return Ok(Some((pos + header, size - header)));
        }
        pos += size;
    }
    Ok(None)
}

/// Big-endian fields read one after another from a box's payload.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
        if len > self.0.len() {
            return Err(invalid("HEIF box ends early"));
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    /// An unsigned number of 0 to 8 bytes; no bytes is 0.
    fn uint(&mut self, len: usize) -> io::Result<u64> {
        Ok(self.take(len)?.iter().fold(0, |n, b| (n << 8) | *b as u64))
    }
}

/// Finds the TIFF block of a HEIF photo: `iinf` names its Exif item and `iloc` says
/// where in the file that item is.
fn heif_tiff_offset<R: Read + Seek>(r: &mut R) -> io::Result<u64> {
    let end = r.seek(SeekFrom::End(0))?;
    let missing = || invalid("HEIF has no EXIF item");
    // `meta` is a full box: its version and flags come before the boxes inside.
    let (meta, len) = find_box(r, 0, end, b"meta")?.ok_or_else(missing)?;
    let (inside, stop) = (meta + 4, meta + len);
    let mut payload = |kind: &[u8; 4]| -> io::Result<Vec<u8>> {
        let (start, len) = find_box(r, inside, stop, kind)?.ok_or_else(missing)?;
        if len > MAX_HEIF_BOX {
            return Err(invalid("HEIF box too large"));
        }
        let mut bytes = vec![0; len as usize];
        r.seek(SeekFrom::Start(start))?;
        r.read_exact(&mut bytes)?;
        Ok(bytes)
    };
    let (iinf, iloc) = (payload(b"iinf")?, payload(b"iloc")?);

    let mut f = Fields(&iinf);
    let version = f.uint(4)? >> 24;
    f.uint(if version == 0 { 2 } else { 4 })?;
    let mut exif_item = None;
    while f.0.len() >= 8 && exif_item.is_none() {
        let size = f.uint(4)? as usize;
        let kind = f.take(4)?;
        let mut entry = Fields(f.take(size.checked_sub(8).ok_or_else(missing)?)?);
        if kind != b"infe" {
            continue;
        }
        let id = match entry.uint(4)? >> 24 {
            2 => entry.uint(2)?,
            3 => entry.uint(4)?,
            _ => continue,
        };
        entry.uint(2)?;
        if entry.take(4)? == b"Exif" {
            exif_item = Some(id);
        }
    }
    let exif_item = exif_item.ok_or_else(missing)?;

    let mut f = Fields(&iloc);
    let version = f.uint(4)? >> 24;
    let sizes = f.uint(2)?;
    let (offset_size, length_size) = ((sizes >> 12) as usize & 15, (sizes >> 8) as usize & 15);
    let (base_size, index_size) = ((sizes >> 4) as usize & 15, sizes as usize & 15);
    let wide = version >= 2;
    for _ in 0..f.uint(if wide { 4 } else { 2 })? {
        let id = f.uint(if wide { 4 } else { 2 })?;
        let in_file = version == 0 || f.uint(2)? & 15 == 0;
        f.uint(2)?;
        let base = f.uint(base_size)?;
        let mut first = None;
        for _ in 0..f.uint(2)? {
            if version > 0 {
                f.uint(index_size)?;
            }
            let offset = f.uint(offset_size)?;
            f.uint(length_size)?;
            first.get_or_insert(offset);
        }
        if id == exif_item {
            let at = first.filter(|_| in_file).ok_or_else(missing)? + base;
            // The item starts with how far in its TIFF block is, past its `Exif` marker.
            let mut skip = [0u8; 4];
            r.seek(SeekFrom::Start(at))?;
            r.read_exact(&mut skip)?;
            return Ok(at + 4 + u32::from_be_bytes(skip) as u64);
        }
    }
    Err(missing())
}

/// Finds the TIFF block inside a JPEG's APP1 "Exif" segment.
fn jpeg_tiff_offset<R: Read + Seek>(r: &mut R) -> io::Result<u64> {
    let mut pos = 2u64;
    loop {
        let mut marker = [0u8; 4];
        r.seek(SeekFrom::Start(pos))?;
        r.read_exact(&mut marker)?;
        if marker[0] != 0xFF {
            return Err(invalid("broken JPEG marker"));
        }
        // Start of scan or end of image: no EXIF before the picture data.
        if marker[1] == 0xDA || marker[1] == 0xD9 {
            return Err(invalid("JPEG has no EXIF block"));
        }
        let len = u16::from_be_bytes([marker[2], marker[3]]) as u64;
        if marker[1] == 0xE1 {
            let mut id = [0u8; 6];
            r.read_exact(&mut id)?;
            if &id == b"Exif\0\0" {
                return Ok(pos + 4 + 6);
            }
        }
        pos += 2 + len;
    }
}

struct Tiff<'a, R> {
    r: &'a mut R,
    base: u64,
    little_endian: bool,
    first_ifd: u32,
}

struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    value: [u8; 4],
}

impl<'a, R: Read + Seek> Tiff<'a, R> {
    fn open(r: &'a mut R, base: u64) -> io::Result<Self> {
        let mut head = [0u8; 8];
        r.seek(SeekFrom::Start(base))?;
        r.read_exact(&mut head)?;
        let little_endian = match &head[..2] {
            b"II" => true,
            b"MM" => false,
            _ => return Err(invalid("unknown TIFF byte order")),
        };
        let mut tiff = Tiff {
            r,
            base,
            little_endian,
            first_ifd: 0,
        };
        if tiff.u16(&head[2..4]) != 42 {
            return Err(invalid("not a TIFF header"));
        }
        tiff.first_ifd = tiff.u32(&head[4..8]);
        Ok(tiff)
    }

    fn u16(&self, b: &[u8]) -> u16 {
        let b = [b[0], b[1]];
        if self.little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        }
    }

    fn u32(&self, b: &[u8]) -> u32 {
        let b = [b[0], b[1], b[2], b[3]];
        if self.little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    }

    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        self.r.seek(SeekFrom::Start(self.base + offset as u64))?;
        self.r.read_exact(buf)
    }

    fn entries(&mut self, offset: u32) -> io::Result<Vec<Entry>> {
        let mut count = [0u8; 2];
        self.read_at(offset, &mut count)?;
        let count = self.u16(&count);
        if count > MAX_ENTRIES {
            return Err(invalid("IFD has too many entries"));
        }
        let mut buf = vec![0u8; count as usize * 12];
        self.read_at(offset + 2, &mut buf)?;
        Ok(buf
            .as_chunks::<12>()
            .0
            .iter()
            .map(|e| Entry {
                tag: self.u16(&e[0..2]),
                kind: self.u16(&e[2..4]),
                count: self.u32(&e[4..8]),
                value: [e[8], e[9], e[10], e[11]],
            })
            .collect())
    }

    fn offset(&self, entries: &[Entry], tag: u16) -> Option<u32> {
        entries
            .iter()
            .find(|e| e.tag == tag)
            .map(|e| self.u32(&e.value))
    }

    fn ascii(&mut self, entries: &[Entry], tag: u16) -> io::Result<Option<String>> {
        let Some(e) = entries
            .iter()
            .find(|e| e.tag == tag && e.kind == TYPE_ASCII)
        else {
            return Ok(None);
        };
        if e.count == 0 || e.count > MAX_STRING {
            return Ok(None);
        }
        let mut buf = vec![0u8; e.count as usize];
        if e.count <= 4 {
            buf.copy_from_slice(&e.value[..e.count as usize]);
        } else {
            let at = self.u32(&e.value);
            self.read_at(at, &mut buf)?;
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim_end_matches('\0').trim();
        Ok((!text.is_empty()).then(|| text.to_string()))
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    /// Builds a minimal TIFF: IFD0 with ASCII tags plus a pointer to an EXIF IFD.
    pub(crate) fn build_tiff(
        little_endian: bool,
        ifd0: &[(u16, &str)],
        exif: &[(u16, &str)],
    ) -> Vec<u8> {
        let w16 = |v: u16| {
            if little_endian {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let w32 = |v: u32| {
            if little_endian {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let ifd0_offset = 8usize;
        let ifd0_size = 2 + 12 * (ifd0.len() + 1) + 4;
        let exif_offset = ifd0_offset + ifd0_size;
        let exif_size = 2 + 12 * exif.len() + 4;
        let mut data_offset = exif_offset + exif_size;
        let mut data = Vec::new();

        let mut write_ascii = |out: &mut Vec<u8>, tag: u16, text: &str| {
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            out.extend(w16(tag));
            out.extend(w16(TYPE_ASCII));
            out.extend(w32(bytes.len() as u32));
            if bytes.len() <= 4 {
                let mut inline = [0u8; 4];
                inline[..bytes.len()].copy_from_slice(&bytes);
                out.extend(inline);
            } else {
                out.extend(w32(data_offset as u32));
                data_offset += bytes.len();
                data.extend(bytes);
            }
        };

        let mut out = Vec::new();
        out.extend(if little_endian { b"II" } else { b"MM" });
        out.extend(w16(42));
        out.extend(w32(ifd0_offset as u32));

        out.extend(w16(ifd0.len() as u16 + 1));
        for (tag, text) in ifd0 {
            write_ascii(&mut out, *tag, text);
        }
        out.extend(w16(TAG_EXIF_IFD));
        out.extend(w16(4));
        out.extend(w32(1));
        out.extend(w32(exif_offset as u32));
        out.extend(w32(0));

        out.extend(w16(exif.len() as u16));
        for (tag, text) in exif {
            write_ascii(&mut out, *tag, text);
        }
        out.extend(w32(0));

        out.extend(data);
        out
    }

    pub(crate) fn sample(little_endian: bool) -> Vec<u8> {
        build_tiff(
            little_endian,
            &[
                (TAG_MAKE, "SONY"),
                (TAG_MODEL, "ILCE-7RM6"),
                (TAG_SOFTWARE, "ILCE-7RM6 v1.00"),
            ],
            &[
                (TAG_DATE_TIME_ORIGINAL, "2026:11:06 17:21:04"),
                (TAG_BODY_SERIAL_NUMBER, "03506694"),
                (TAG_LENS_MODEL, "FE 35mm F1.4 GM"),
            ],
        )
    }

    /// A Nikon Z 9 raw: a little-endian TIFF like any NEF, with the tags Nikon writes.
    pub(crate) fn nikon_sample() -> Vec<u8> {
        build_tiff(
            true,
            &[
                (TAG_MAKE, "NIKON CORPORATION"),
                (TAG_MODEL, "NIKON Z 9"),
                (TAG_SOFTWARE, "Ver.05.30"),
            ],
            &[
                (TAG_DATE_TIME_ORIGINAL, "2026:11:07 09:15:22"),
                (TAG_BODY_SERIAL_NUMBER, "6001234"),
                (TAG_LENS_MODEL, "NIKKOR Z 24-70mm f/2.8 S"),
            ],
        )
    }

    /// A Hasselblad X2D II 3FR (its JPEG has the same tags): a little-endian TIFF with
    /// the serial in IFD0's CameraSerialNumber and no BodySerialNumber.
    pub(crate) fn hasselblad_sample() -> Vec<u8> {
        build_tiff(
            true,
            &[
                (TAG_MAKE, "Hasselblad"),
                (TAG_MODEL, "X2D II 100C"),
                (TAG_SOFTWARE, "1.3.16.2"),
                (TAG_CAMERA_SERIAL_NUMBER, "XU25000577"),
            ],
            &[
                (TAG_DATE_TIME_ORIGINAL, "2026:11:15 15:42:53"),
                (TAG_LENS_MODEL, "XCD 55V"),
            ],
        )
    }

    fn expected() -> BasicExif {
        BasicExif {
            make: Some("SONY".into()),
            model: Some("ILCE-7RM6".into()),
            software: Some("ILCE-7RM6 v1.00".into()),
            serial: Some("03506694".into()),
            lens: Some("FE 35mm F1.4 GM".into()),
            captured: NaiveDateTime::parse_from_str("2026-11-06 17:21:04", "%Y-%m-%d %H:%M:%S")
                .ok(),
        }
    }

    #[test]
    fn reads_little_endian_tiff() {
        let got = read_basic_from(&mut Cursor::new(sample(true))).unwrap();
        assert_eq!(got, expected());
    }

    #[test]
    fn reads_big_endian_tiff() {
        let got = read_basic_from(&mut Cursor::new(sample(false))).unwrap();
        assert_eq!(got, expected());
    }

    #[test]
    fn reads_a_nikon_z_9_nef() {
        // NEF is a TIFF container, so the same reader serves Sony, Nikon and DNG files.
        let got = read_basic_from(&mut Cursor::new(nikon_sample())).unwrap();
        assert_eq!(got.make.as_deref(), Some("NIKON CORPORATION"));
        assert_eq!(got.model.as_deref(), Some("NIKON Z 9"));
        assert_eq!(got.serial.as_deref(), Some("6001234"));
        assert_eq!(got.lens.as_deref(), Some("NIKKOR Z 24-70mm f/2.8 S"));
        assert_eq!(
            got.captured.map(|t| t.to_string()).as_deref(),
            Some("2026-11-07 09:15:22")
        );
    }

    #[test]
    fn reads_a_hasselblad_serial_from_ifd0() {
        let got = read_basic_from(&mut Cursor::new(hasselblad_sample())).unwrap();
        assert_eq!(got.model.as_deref(), Some("X2D II 100C"));
        assert_eq!(got.serial.as_deref(), Some("XU25000577"));
        assert_eq!(
            got.captured.map(|t| t.to_string()).as_deref(),
            Some("2026-11-15 15:42:53")
        );
        // BodySerialNumber still wins where a camera writes both.
        let both = build_tiff(
            true,
            &[(TAG_CAMERA_SERIAL_NUMBER, "OTHER")],
            &[(TAG_BODY_SERIAL_NUMBER, "03506694")],
        );
        let got = read_basic_from(&mut Cursor::new(both)).unwrap();
        assert_eq!(got.serial.as_deref(), Some("03506694"));
    }

    /// A JPEG holding `tiff` as its EXIF block.
    pub(crate) fn jpeg(tiff: &[u8]) -> Vec<u8> {
        let mut jpeg = vec![0xFF, 0xD8];
        // An APP0 segment first, as most JPEGs have.
        jpeg.extend([0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        let app1_len = (2 + 6 + tiff.len()) as u16;
        jpeg.extend([0xFF, 0xE1]);
        jpeg.extend(app1_len.to_be_bytes());
        jpeg.extend(b"Exif\0\0");
        jpeg.extend(tiff);
        jpeg.extend([0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn reads_exif_inside_a_jpeg() {
        let got = read_basic_from(&mut Cursor::new(jpeg(&sample(true)))).unwrap();
        assert_eq!(got, expected());
    }

    /// A HEIF photo as a camera lays it out: `ftyp`, `meta` with `iinf` and `iloc`,
    /// then `mdat` holding the Exif item.
    pub(crate) fn heif(tiff: &[u8]) -> Vec<u8> {
        fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
            out.extend(kind);
            out.extend(payload);
            out
        }
        let mut infe = vec![2, 0, 0, 0, 0, 13, 0, 0];
        infe.extend(b"Exif\0");
        let mut iinf = vec![0, 0, 0, 0, 0, 1];
        iinf.extend(boxed(b"infe", &infe));
        let mut exif = vec![0, 0, 0, 6];
        exif.extend(b"Exif\0\0");
        exif.extend(tiff);
        let meta = |offset: u32| {
            // Version 1, 4-byte offsets and lengths, one item (13) with one extent.
            let mut iloc = vec![1, 0, 0, 0, 0x44, 0x00, 0, 1, 0, 13, 0, 0, 0, 0, 0, 1];
            iloc.extend(offset.to_be_bytes());
            iloc.extend((exif.len() as u32).to_be_bytes());
            let mut inside = vec![0, 0, 0, 0];
            inside.extend(boxed(b"hdlr", &[0; 12]));
            inside.extend(boxed(b"iinf", &iinf));
            inside.extend(boxed(b"iloc", &iloc));
            boxed(b"meta", &inside)
        };
        let mut out = boxed(b"ftyp", b"heix\0\0\0\0mif1heix");
        let at = (out.len() + meta(0).len() + 8) as u32;
        out.extend(meta(at));
        out.extend(boxed(b"mdat", &exif));
        out
    }

    #[test]
    fn reads_exif_inside_a_heif_photo() {
        let got = read_basic_from(&mut Cursor::new(heif(&sample(true)))).unwrap();
        assert_eq!(got, expected());
        // Without an Exif item, or cut short, it's an error and not a guess.
        let mut none = heif(&sample(true));
        let at = none.windows(4).position(|w| w == b"Exif").unwrap();
        none[at..at + 4].copy_from_slice(b"mime");
        assert!(read_basic_from(&mut Cursor::new(none)).is_err());
        let whole = heif(&sample(true));
        assert!(read_basic_from(&mut Cursor::new(whole[..60].to_vec())).is_err());
    }

    #[test]
    fn tells_a_photo_from_other_bytes_under_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let (tiff, heif) = (sample(true), heif(&sample(true)));
        assert!(content_matches(&file("a.ARW", &tiff)).unwrap());
        assert!(content_matches(&file("a.nef", &sample(false))).unwrap());
        assert!(content_matches(&file("a.JPG", &[0xFF, 0xD8, 0xFF, 0xE1])).unwrap());
        assert!(content_matches(&file("a.HIF", &heif)).unwrap());
        // Another photo's bytes, the middle of a raw, an empty file.
        assert!(!content_matches(&file("b.JPG", &tiff)).unwrap());
        assert!(!content_matches(&file("b.HIF", &tiff)).unwrap());
        assert!(!content_matches(&file("b.ARW", &[0x7D, 0xF7, 0xDF, 0x7D, 0xF7])).unwrap());
        assert!(!content_matches(&file("c.ARW", b"")).unwrap());
        // Clips and sidecars aren't judged here.
        assert!(content_matches(&file("c.MP4", b"clip")).unwrap());
    }

    #[test]
    fn rejects_other_files() {
        let err = read_basic_from(&mut Cursor::new(b"not an image".to_vec())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn missing_exif_ifd_gives_no_date() {
        let mut bytes = build_tiff(true, &[(TAG_MODEL, "X")], &[]);
        // Point the EXIF IFD tag at a tag number nobody asks for.
        let pos = 8 + 2 + 12;
        bytes[pos..pos + 2].copy_from_slice(&0x1234u16.to_le_bytes());
        let got = read_basic_from(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(got.model.as_deref(), Some("X"));
        assert_eq!(got.captured, None);
    }
}
