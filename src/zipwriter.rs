//! Zip writer tối giản (method "stored", không nén) để gói nhiều file mp4 output
//! của các preset thành 1 link tải — tránh thêm dependency chỉ vì việc này.
//!
//! Mp4 đã nén sẵn nên "stored" gần như không mất gì về kích thước. Định dạng
//! được viết theo APPNOTE.TXT: local file header + central directory + EOCD,
//! CRC32 chuẩn (polynomial 0xEDB88320).

use std::io;

// ---------------------------------------------------------------------------
// CRC32
// ---------------------------------------------------------------------------

/// Bảng CRC32 dựng lúc đầu tiên (OnceLock để không tính lại mỗi lần zip).
fn crc_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            table[i as usize] = c;
        }
        table
    })
}

fn crc32(data: &[u8]) -> u32 {
    let table = crc_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// Zip writer
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CentralEntry {
    name: Vec<u8>,
    crc: u32,
    size: u32,
    offset: u32,
}

/// Writer zip stream: ghi lần lượt từng file rồi `finish()` để chốt.
pub struct ZipWriter<W: io::Write> {
    out: W,
    entries: Vec<CentralEntry>,
}

impl<W: io::Write> ZipWriter<W> {
    pub fn new(out: W) -> Self {
        Self { out, entries: Vec::new() }
    }

    /// Thêm 1 file từ dữ liệu có sẵn (caller tự đọc — xem
    /// `jobs::read_with_fallback`). Mp4 output luôn < 4GB nên u32 size là đủ
    /// (zip32).
    pub fn add_bytes(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        if data.len() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "File quá lớn cho zip32 (>= 4GB).",
            ));
        }
        let name_bytes = name.as_bytes().to_vec();
        if name_bytes.len() > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Tên file trong zip quá dài.",
            ));
        }

        let offset = self.entries.iter().map(|e| 30 + e.name.len() + e.size as usize).sum::<usize>() as u64;
        if offset > u32::MAX as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Zip vượt giới hạn 4GB của zip32.",
            ));
        }
        let offset = offset as u32;

        let crc = crc32(data);
        let size = data.len() as u32;

        // ---- Local file header ----
        let mut hdr = Vec::with_capacity(30);
        put_u32(&mut hdr, 0x0403_4B50); // signature
        put_u16(&mut hdr, 20); // version needed
        put_u16(&mut hdr, 0x0800); // flags: UTF-8 tên file
        put_u16(&mut hdr, 0); // method: stored
        put_u16(&mut hdr, 0); // mod time
        put_u16(&mut hdr, 0x5A21); // mod date (1996-01-01, mtime 0)
        put_u32(&mut hdr, crc);
        put_u32(&mut hdr, size); // compressed
        put_u32(&mut hdr, size); // uncompressed
        put_u16(&mut hdr, name_bytes.len() as u16);
        put_u16(&mut hdr, 0); // extra length
        self.out.write_all(&hdr)?;
        self.out.write_all(&name_bytes)?;
        self.out.write_all(data)?;

        self.entries.push(CentralEntry {
            name: name_bytes,
            crc,
            size,
            offset,
        });
        Ok(())
    }

    /// Chốt zip: central directory + EOCD. Trả về writer bên trong.
    pub fn finish(mut self) -> io::Result<W> {
        let dir_offset: u32 = self
            .entries
            .iter()
            .map(|e| 30 + e.name.len() + e.size as usize)
            .sum::<usize>() as u32;
        let mut dir_size: u32 = 0;

        for e in &self.entries {
            let mut c = Vec::with_capacity(46);
            put_u32(&mut c, 0x0201_4B50); // signature
            put_u16(&mut c, 20); // version made by
            put_u16(&mut c, 20); // version needed
            put_u16(&mut c, 0x0800); // flags: UTF-8
            put_u16(&mut c, 0); // method
            put_u16(&mut c, 0); // mod time
            put_u16(&mut c, 0x5A21); // mod date
            put_u32(&mut c, e.crc);
            put_u32(&mut c, e.size);
            put_u32(&mut c, e.size);
            put_u16(&mut c, e.name.len() as u16);
            put_u16(&mut c, 0); // extra
            put_u16(&mut c, 0); // comment
            put_u16(&mut c, 0); // disk number
            put_u16(&mut c, 0); // internal attrs
            put_u32(&mut c, 0); // external attrs
            put_u32(&mut c, e.offset);
            debug_assert_eq!(c.len(), 46);
            self.out.write_all(&c)?;
            self.out.write_all(&e.name)?;
            dir_size += 46 + e.name.len() as u32;
        }

        let mut eocd = Vec::with_capacity(22);
        put_u32(&mut eocd, 0x0605_4B50);
        put_u16(&mut eocd, 0); // disk
        put_u16(&mut eocd, 0); // disk với central dir
        put_u16(&mut eocd, self.entries.len() as u16);
        put_u16(&mut eocd, self.entries.len() as u16);
        put_u32(&mut eocd, dir_size);
        put_u32(&mut eocd, dir_offset);
        put_u16(&mut eocd, 0); // comment length
        self.out.write_all(&eocd)?;

        self.out.flush()?;
        Ok(self.out)
    }
}

fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC32 chuẩn của chuỗi rỗng và "123456789" (giá trị kiểm định từ spec).
    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn zip_magic_and_eocd_present() {
        let mut w = ZipWriter::new(Vec::new());
        w.add_bytes("a.txt", b"hello").unwrap();
        w.add_bytes("dir/b.bin", &[0u8, 1, 2, 255]).unwrap();
        let out = w.finish().unwrap();

        // Local header magic.
        assert_eq!(&out[0..4], &[0x50, 0x4B, 0x03, 0x04]);
        // EOCD ở cuối 22 bytes; "total entries" nằm ở byte 8–10 của EOCD.
        let n = out.len();
        assert_eq!(&out[n - 22..n - 18], &[0x50, 0x4B, 0x05, 0x06]);
        assert_eq!(u16::from_le_bytes([out[n - 14], out[n - 13]]), 2);
        // Nội dung nằm ở đúng vị trí trong stream.
        let a_pos = out.windows(5).position(|w| w == b"a.txt").unwrap();
        assert_eq!(&out[a_pos + 5..a_pos + 10], b"hello");
    }

    #[test]
    fn zip_two_files_offsets_consistent() {
        let mut w = ZipWriter::new(Vec::new());
        w.add_bytes("one.mp4", &[7u8; 100]).unwrap();
        w.add_bytes("two.mp4", &[9u8; 5]).unwrap();
        let out = w.finish().unwrap();

        // Entry 2 offset trong central dir phải trỏ tới local header thứ hai.
        // Local 1: 30 + 7 + 100 = 137 bytes.
        // Central 2 bắt đầu sau central 1 (46 + 7) + EOCD không tính.
        let n = out.len();
        let central2 = n - 22 - (46 + 7); // central dir: 2 entries, entry2 ở cuối
        let off_bytes = &out[central2 + 42..central2 + 46];
        assert_eq!(u32::from_le_bytes(off_bytes.try_into().unwrap()), 137);
    }
}
