//! A small EXIF reader for the fields Card Intake needs: camera make, model, serial
//! number, firmware, lens and capture time.
//!
//! Reads TIFF-based raws (ARW, DNG, and Nikon's NEF/NRW) and JPEGs. It seeks straight
//! to the few tags it needs, so it reads a few KB per file instead of the whole 60 MB
//! raw.

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

const TYPE_ASCII: u16 = 2;
const MAX_ENTRIES: u16 = 1024;
const MAX_STRING: u32 = 512;

pub fn read_basic(path: &Path) -> io::Result<BasicExif> {
    read_basic_from(&mut BufReader::new(File::open(path)?))
}

pub fn read_basic_from<R: Read + Seek>(r: &mut R) -> io::Result<BasicExif> {
    let mut magic = [0u8; 2];
    r.seek(SeekFrom::Start(0))?;
    r.read_exact(&mut magic)?;
    let base = match magic {
        [b'I', b'I'] | [b'M', b'M'] => 0,
        [0xFF, 0xD8] => jpeg_tiff_offset(r)?,
        _ => return Err(invalid("not a TIFF-based raw or a JPEG")),
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
    Ok(out)
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
    fn reads_exif_inside_a_jpeg() {
        let tiff = sample(true);
        let mut jpeg = vec![0xFF, 0xD8];
        // An APP0 segment first, as most JPEGs have.
        jpeg.extend([0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        let app1_len = (2 + 6 + tiff.len()) as u16;
        jpeg.extend([0xFF, 0xE1]);
        jpeg.extend(app1_len.to_be_bytes());
        jpeg.extend(b"Exif\0\0");
        jpeg.extend(&tiff);
        jpeg.extend([0xFF, 0xD9]);

        let got = read_basic_from(&mut Cursor::new(jpeg)).unwrap();
        assert_eq!(got, expected());
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
