//! Android Boot Image v3/v4 unpacker and Linux bootconfig initramfs assembler.
//!
//! Unpacks:
//!   - `boot.img`        -> raw ARM64 Linux kernel Image (gunzipped if needed)
//!   - `init_boot.img`   -> generic Android first-stage ramdisk
//!   - `vendor_boot.img` -> Cuttlefish vendor ramdisk (virtio*.ko + fstab), cmdline, bootconfig
//!
//! Ramdisks are decompressed on the host into plain `newc` cpio archives. The
//! kernel's in-place lz4 decoder aborts the whole initramfs unpack at the first
//! byte it cannot parse as an lz4 chunk (padding between fragments, an
//! uncompressed overlay after an lz4 stream, ...), silently dropping every later
//! archive. Concatenated uncompressed cpio archives have no such pitfall.

use anyhow::{bail, Context, Result};
use flate2::read::{GzDecoder, MultiGzDecoder};
use std::fs;
use std::io::Read;
use std::path::Path;

const BOOT_MAGIC: &[u8; 8] = b"ANDROID!";
const VENDOR_BOOT_MAGIC: &[u8; 8] = b"VNDRBOOT";
const BOOT_V4_PAGE_SIZE: usize = 4096;
const VENDOR_RAMDISK_TYPE_RECOVERY: u32 = 2;
const BOOTCONFIG_MAGIC: &[u8; 12] = b"#BOOTCONFIG\n";
const LZ4_LEGACY_MAGIC: u32 = 0x184C_2102;
const LZ4_FRAME_MAGIC: u32 = 0x184D_2204;
/// `lz4 -l` legacy frames hold independent blocks of at most 8 MiB each.
const LZ4_LEGACY_BLOCK_MAX: usize = 8 << 20;
const CPIO_NEWC_MAGIC: &[u8; 6] = b"070701";

fn read_u32_le(buf: &[u8], off: usize) -> Result<u32> {
    let slice = buf
        .get(off..off + 4)
        .with_context(|| format!("truncated image reading u32 at offset {off}"))?;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

fn align_up(n: usize, align: usize) -> usize {
    if align == 0 {
        return n;
    }
    (n + align - 1) / align * align
}

fn cstr_from_bytes(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

pub struct BootImgParts {
    pub kernel: Vec<u8>,
    pub ramdisk: Vec<u8>,
    #[allow(dead_code)]
    pub cmdline: String,
}

pub struct VendorBootParts {
    /// Uncompressed concatenation of all non-recovery vendor ramdisk fragments.
    pub ramdisk: Vec<u8>,
    pub cmdline: String,
    pub bootconfig: String,
}

/// Decode one LZ4 block (no frame) and append the result to `out`.
fn lz4_decompress_block(src: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let base = out.len();
    let mut i = 0usize;
    let read_len = |i: &mut usize, mut n: usize| -> Result<usize> {
        loop {
            let b = *src.get(*i).context("lz4: truncated length")?;
            *i += 1;
            n += b as usize;
            if b != 255 {
                return Ok(n);
            }
        }
    };
    while i < src.len() {
        let token = src[i];
        i += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            lit = read_len(&mut i, lit)?;
        }
        let lit_end = i.checked_add(lit).filter(|&e| e <= src.len()).context("lz4: literal overrun")?;
        out.extend_from_slice(&src[i..lit_end]);
        i = lit_end;
        if i == src.len() {
            break; // the last sequence carries literals only
        }
        let off = u16::from_le_bytes(
            src.get(i..i + 2).context("lz4: truncated offset")?.try_into().unwrap(),
        ) as usize;
        i += 2;
        if off == 0 || off > out.len() - base {
            bail!("lz4: invalid match offset {off}");
        }
        let mut mlen = (token & 15) as usize;
        if mlen == 15 {
            mlen = read_len(&mut i, mlen)?;
        }
        mlen += 4;
        let from = out.len() - off;
        if off >= mlen {
            out.extend_from_within(from..from + mlen);
        } else {
            // Overlapping match: copy byte by byte (run-length style).
            for k in 0..mlen {
                let b = out[from + k];
                out.push(b);
            }
        }
    }
    Ok(())
}

/// Decode one or more concatenated `lz4 -l` legacy streams.
fn lz4_legacy_decompress(src: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(src.len() * 3);
    let mut p = 0usize;
    let mut in_stream = false;
    while p < src.len() {
        if !in_stream {
            // Tolerate zero padding between concatenated streams.
            if src[p] == 0 {
                p += 1;
                continue;
            }
            if read_u32_le(src, p)? != LZ4_LEGACY_MAGIC {
                bail!("lz4: expected legacy magic at offset {p}");
            }
            p += 4;
            in_stream = true;
            continue;
        }
        if src.len() - p < 4 {
            break;
        }
        let chunk = read_u32_le(src, p)? as usize;
        if chunk == LZ4_LEGACY_MAGIC as usize {
            p += 4;
            continue;
        }
        if chunk == 0 {
            in_stream = false;
            continue;
        }
        let end = p + 4 + chunk;
        if chunk > LZ4_LEGACY_BLOCK_MAX + LZ4_LEGACY_BLOCK_MAX / 255 + 16 || end > src.len() {
            // Zero padding followed by another stream (or the end) is not a chunk.
            let zeros = src[p..].iter().take_while(|&&b| b == 0).count();
            let q = p + zeros;
            if zeros > 0 && (q == src.len() || read_u32_le(src, q).ok() == Some(LZ4_LEGACY_MAGIC)) {
                p = q;
                in_stream = false;
                continue;
            }
            bail!("lz4: bad chunk size {chunk:#x} at offset {p}");
        }
        lz4_decompress_block(&src[p + 4..end], &mut out)?;
        p = end;
    }
    Ok(out)
}

/// Turn an Android ramdisk (lz4 legacy, gzip, or already-uncompressed cpio)
/// into an uncompressed `newc` cpio stream.
pub fn decompress_ramdisk(data: &[u8]) -> Result<Vec<u8>> {
    if data.starts_with(CPIO_NEWC_MAGIC) {
        return Ok(data.to_vec());
    }
    if data.len() >= 4 && read_u32_le(data, 0)? == LZ4_LEGACY_MAGIC {
        return lz4_legacy_decompress(data);
    }
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        let mut out = Vec::with_capacity(data.len() * 3);
        MultiGzDecoder::new(data)
            .read_to_end(&mut out)
            .context("gunzipping ramdisk")?;
        return Ok(out);
    }
    if data.len() >= 4 && read_u32_le(data, 0)? == LZ4_FRAME_MAGIC {
        bail!("ramdisk uses the LZ4 frame format; rebuild with legacy lz4 (`lz4 -l`) or gzip");
    }
    bail!(
        "unrecognized ramdisk compression (magic {:02x?})",
        &data[..data.len().min(4)]
    )
}

/// Parse an Android v3/v4 `boot.img` or `init_boot.img`.
pub fn parse_boot_img(path: &Path) -> Result<BootImgParts> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if data.len() < 1580 || &data[0..8] != BOOT_MAGIC {
        bail!("{}: not a valid Android boot image (missing ANDROID! magic)", path.display());
    }
    let kernel_size = read_u32_le(&data, 8)? as usize;
    let ramdisk_size = read_u32_le(&data, 12)? as usize;
    let header_version = read_u32_le(&data, 40)?;
    if header_version != 3 && header_version != 4 {
        bail!(
            "{}: unsupported boot header version {header_version} (expected 3 or 4)",
            path.display()
        );
    }
    let cmdline = cstr_from_bytes(&data[44..44 + 1536]);

    let kernel_off = BOOT_V4_PAGE_SIZE;
    let kernel_end = kernel_off + kernel_size;
    if kernel_end > data.len() {
        bail!("{}: kernel exceeds image size", path.display());
    }
    let raw_kernel = &data[kernel_off..kernel_end];
    let kernel = if raw_kernel.len() >= 2 && raw_kernel[0] == 0x1f && raw_kernel[1] == 0x8b {
        let mut dec = GzDecoder::new(raw_kernel);
        let mut out = Vec::new();
        dec.read_to_end(&mut out)
            .with_context(|| format!("gunzipping kernel in {}", path.display()))?;
        out
    } else {
        raw_kernel.to_vec()
    };

    let ramdisk_off = kernel_off + align_up(kernel_size, BOOT_V4_PAGE_SIZE);
    let ramdisk_end = ramdisk_off + ramdisk_size;
    if ramdisk_end > data.len() {
        bail!("{}: ramdisk exceeds image size", path.display());
    }
    let ramdisk = data[ramdisk_off..ramdisk_end].to_vec();

    Ok(BootImgParts { kernel, ramdisk, cmdline })
}

/// Parse an Android v3/v4 `vendor_boot.img`.
/// If a v4 `vendor_ramdisk_table` is present, skips RECOVERY fragments so the
/// device always performs a normal Android boot. Every fragment is decompressed
/// separately and the resulting cpio archives are concatenated.
pub fn parse_vendor_boot_img(path: &Path) -> Result<VendorBootParts> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if data.len() < 2112 || &data[0..8] != VENDOR_BOOT_MAGIC {
        bail!(
            "{}: not a valid vendor_boot image (missing VNDRBOOT magic)",
            path.display()
        );
    }
    let header_version = read_u32_le(&data, 8)?;
    if header_version != 3 && header_version != 4 {
        bail!(
            "{}: unsupported vendor_boot header version {header_version} (expected 3 or 4)",
            path.display()
        );
    }
    let page_size = read_u32_le(&data, 12)? as usize;
    if page_size == 0 || !page_size.is_power_of_two() {
        bail!("{}: invalid page_size {page_size}", path.display());
    }
    let vendor_ramdisk_size = read_u32_le(&data, 24)? as usize;
    let cmdline = cstr_from_bytes(&data[28..28 + 2048]);
    let header_size = read_u32_le(&data, 2096)? as usize;
    let dtb_size = read_u32_le(&data, 2100)? as usize;

    let ramdisk_off = align_up(header_size, page_size);
    let ramdisk_end = ramdisk_off + vendor_ramdisk_size;
    if ramdisk_end > data.len() {
        bail!("{}: vendor_ramdisk exceeds image size", path.display());
    }
    let raw_vendor_ramdisk = &data[ramdisk_off..ramdisk_end];

    let (table_size, entry_num, entry_size, bootconfig_size) = if header_version == 4 {
        (
            read_u32_le(&data, 2112)? as usize,
            read_u32_le(&data, 2116)? as usize,
            read_u32_le(&data, 2120)? as usize,
            read_u32_le(&data, 2124)? as usize,
        )
    } else {
        (0, 0, 0, 0)
    };

    let dtb_off = ramdisk_off + align_up(vendor_ramdisk_size, page_size);
    let table_off = dtb_off + align_up(dtb_size, page_size);
    let bootconfig_off = table_off + align_up(table_size, page_size);

    let ramdisk = if header_version == 4 && entry_num > 0 && entry_size >= 12 {
        let mut combined = Vec::new();
        for i in 0..entry_num {
            let e = table_off + i * entry_size;
            let f_size = read_u32_le(&data, e)? as usize;
            let f_off = read_u32_le(&data, e + 4)? as usize;
            let f_type = read_u32_le(&data, e + 8)?;
            if f_type == VENDOR_RAMDISK_TYPE_RECOVERY {
                continue;
            }
            let end = f_off + f_size;
            if end > raw_vendor_ramdisk.len() {
                bail!("{}: ramdisk table entry {i} out of bounds", path.display());
            }
            let cpio = decompress_ramdisk(&raw_vendor_ramdisk[f_off..end])
                .with_context(|| format!("{}: vendor ramdisk fragment {i}", path.display()))?;
            pad4(&mut combined);
            combined.extend_from_slice(&cpio);
        }
        combined
    } else {
        decompress_ramdisk(raw_vendor_ramdisk)
            .with_context(|| format!("{}: vendor ramdisk", path.display()))?
    };

    let bootconfig = if bootconfig_size > 0 && bootconfig_off + bootconfig_size <= data.len() {
        let raw_bc = &data[bootconfig_off..bootconfig_off + bootconfig_size];
        String::from_utf8_lossy(raw_bc).trim_end_matches('\0').to_string()
    } else {
        String::new()
    };

    Ok(VendorBootParts { ramdisk, cmdline, bootconfig })
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

/// Build an uncompressed `newc` cpio archive containing a single regular file.
pub fn build_single_file_cpio(name: &str, mode: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 512);
    write_cpio_entry(&mut out, name, 0o100000 | (mode & 0o7777), 1, data);
    write_cpio_entry(&mut out, "TRAILER!!!", 0, 1, &[]);
    pad4(&mut out);
    out
}

fn write_cpio_entry(out: &mut Vec<u8>, name: &str, mode: u32, nlink: u32, data: &[u8]) {
    let name_bytes = name.as_bytes();
    let namesize = name_bytes.len() + 1;
    let header = format!(
        "070701\
         {:08X}{:08X}{:08X}{:08X}\
         {:08X}{:08X}{:08X}{:08X}\
         {:08X}{:08X}{:08X}{:08X}\
         {:08X}",
        1u32,               // ino
        mode,               // mode
        0u32,               // uid
        0u32,               // gid
        nlink,              // nlink
        0u32,               // mtime
        data.len() as u32,  // filesize
        0u32,               // devmajor
        0u32,               // devminor
        0u32,               // rdevmajor
        0u32,               // rdevminor
        namesize as u32,    // namesize
        0u32,               // check
    );
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(name_bytes);
    out.push(0);
    pad4(out);
    out.extend_from_slice(data);
    pad4(out);
}

/// Concatenate base ramdisks, optional extra cpio overlays, and append a valid
/// Linux kernel `#BOOTCONFIG\n` trailer.
///
/// The kernel requires `bootconfig` in `/proc/cmdline` to parse this trailer.
pub fn assemble_initrd_with_bootconfig(
    vendor_ramdisk: &[u8],
    generic_ramdisk: &[u8],
    extra_cpio: Option<&[u8]>,
    bootconfig_text: &str,
) -> Vec<u8> {
    let extra_len = extra_cpio.map_or(0, |e| e.len());
    let mut out = Vec::with_capacity(
        extra_len + vendor_ramdisk.len() + generic_ramdisk.len() + bootconfig_text.len() + 64,
    );
    // Place uncompressed `newc` cpio overlays FIRST (standard Linux early-cpio layout).
    // Placing uncompressed bytes after LZ4 legacy streams (0x184c2102) breaks Linux `unlz4`
    // because `unlz4` reads `"0707"` as the next chunk length instead of stopping cleanly.
    if let Some(extra) = extra_cpio {
        out.extend_from_slice(extra);
        pad4(&mut out);
    }
    out.extend_from_slice(vendor_ramdisk);
    pad4(&mut out);
    out.extend_from_slice(generic_ramdisk);
    pad4(&mut out);

    // Ensure bootconfig ends with '\n' and is 4-byte aligned with NUL padding.
    let mut bc = bootconfig_text.as_bytes().to_vec();
    if !bc.ends_with(b"\n") {
        bc.push(b'\n');
    }
    while bc.len() % 4 != 0 {
        bc.push(0);
    }

    let size = bc.len() as u32;
    let csum: u32 = bc.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32));

    out.extend_from_slice(&bc);
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&csum.to_le_bytes());
    out.extend_from_slice(BOOTCONFIG_MAGIC);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootconfig_trailer_has_valid_size_csum_and_magic() {
        let initrd = assemble_initrd_with_bootconfig(b"VND", b"GEN", None, "androidboot.slot_suffix=_a\n");
        let n = initrd.len();
        assert_eq!(&initrd[n - 12..], BOOTCONFIG_MAGIC);
        let csum = u32::from_le_bytes(initrd[n - 16..n - 12].try_into().unwrap());
        let size = u32::from_le_bytes(initrd[n - 20..n - 16].try_into().unwrap()) as usize;
        assert_eq!(size % 4, 0);
        let bc = &initrd[n - 20 - size..n - 20];
        let expected_csum: u32 = bc.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32));
        assert_eq!(csum, expected_csum);
        assert!(std::str::from_utf8(bc).unwrap().starts_with("androidboot.slot_suffix=_a\n"));
    }

    /// `lz4 -l -9` output for LZ4_PLAIN (literals, a 6-byte back-reference
    /// and overlapping run-length matches).
    const LZ4_LEGACY: &[u8] = &[
        0x02, 0x21, 0x4c, 0x18, 0x24, 0x00, 0x00, 0x00, 0x6f, 0x68, 0x65, 0x6c, 0x6c, 0x6f,
        0x20, 0x06, 0x00, 0x05, 0xae, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x21, 0x20, 0x61, 0x62,
        0x63, 0x03, 0x00, 0xc0, 0x20, 0x30, 0x37, 0x30, 0x37, 0x30, 0x31, 0x30, 0x30, 0x30,
        0x30, 0x0a,
    ];
    const LZ4_PLAIN: &[u8] =
        b"hello hello hello hello hello world! abcabcabcabcabcabcabc 0707010000\n";

    #[test]
    fn decodes_lz4_legacy_stream() {
        assert_eq!(decompress_ramdisk(LZ4_LEGACY).unwrap(), LZ4_PLAIN);
    }

    #[test]
    fn decodes_concatenated_lz4_streams_with_zero_padding() {
        let mut two = LZ4_LEGACY.to_vec();
        two.extend_from_slice(&[0, 0, 0]);
        two.extend_from_slice(LZ4_LEGACY);
        let mut expected = LZ4_PLAIN.to_vec();
        expected.extend_from_slice(LZ4_PLAIN);
        assert_eq!(decompress_ramdisk(&two).unwrap(), expected);
    }

    #[test]
    fn rejects_corrupt_lz4() {
        let mut bad = LZ4_LEGACY.to_vec();
        bad[17] = 0xff; // match offset far beyond the decoded output
        assert!(decompress_ramdisk(&bad).is_err());
    }

    #[test]
    fn passes_through_uncompressed_cpio_and_gunzips() {
        let cpio = build_single_file_cpio("x", 0o644, b"data");
        assert_eq!(decompress_ramdisk(&cpio).unwrap(), cpio);

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &cpio).unwrap();
        assert_eq!(decompress_ramdisk(&gz.finish().unwrap()).unwrap(), cpio);
    }
}
