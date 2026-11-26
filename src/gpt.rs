//! Pure-Rust Android sparse image decoder + UEFI GPT disk builder.
//!
//! Creates a sparse GPT `disk.img` containing standard Cuttlefish partitions
//! (`misc`, `frp`, `boot_a`, `init_boot_a`, `vendor_boot_a`, `vbmeta*_a`, `super`,
//! `metadata`, `userdata`, `custom`) without requiring external `sgdisk` or `simg2img`.

use anyhow::{bail, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const SECTOR_SIZE: u64 = 512;
const ALIGN_BYTES: u64 = 1024 * 1024; // 1 MiB alignment
const ALIGN_SECTORS: u64 = ALIGN_BYTES / SECTOR_SIZE;
const GPT_ENTRIES: u32 = 128;
const GPT_ENTRY_SIZE: u32 = 128;
const GPT_ENTRIES_SECTORS: u64 = (GPT_ENTRIES as u64 * GPT_ENTRY_SIZE as u64) / SECTOR_SIZE;

// Linux filesystem data GUID: 0FC63DAF-8483-4772-8E79-3D69D8477DE4 (mixed-endian GPT format)
const LINUX_FS_TYPE_GUID: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47,
    0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

const SPARSE_MAGIC: u32 = 0xED26_FF3A;
const CHUNK_RAW: u16 = 0xCAC1;
const CHUNK_FILL: u16 = 0xCAC2;
const CHUNK_DONT_CARE: u16 = 0xCAC3;
const CHUNK_CRC32: u16 = 0xCAC4;

pub enum PartitionSource<'a> {
    /// Copy raw or Android sparse image file into the partition.
    File(&'a Path),
    /// Leave partition zeroed (sparse holes on APFS); Android `formattable` formats it on boot.
    Empty { size_bytes: u64 },
}

pub struct PartitionSpec<'a> {
    pub name: &'a str,
    pub source: PartitionSource<'a>,
    /// Optional minimum size; partition is sized to max(min_size, image_uncompressed_size).
    pub min_size_bytes: u64,
}

fn read_sparse_header(f: &mut File) -> Result<Option<(u32, u32, u32, u16, u16)>> {
    f.seek(SeekFrom::Start(0))?;
    let mut hdr = [0u8; 28];
    if f.read(&mut hdr)? < 28 {
        return Ok(None);
    }
    let magic = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
    if magic != SPARSE_MAGIC {
        return Ok(None);
    }
    let file_hdr_sz = u16::from_le_bytes(hdr[8..10].try_into().unwrap());
    let chunk_hdr_sz = u16::from_le_bytes(hdr[10..12].try_into().unwrap());
    let blk_sz = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
    let total_blks = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
    let total_chunks = u32::from_le_bytes(hdr[20..24].try_into().unwrap());
    Ok(Some((blk_sz, total_blks, total_chunks, file_hdr_sz, chunk_hdr_sz)))
}

/// Return the uncompressed byte size of a raw or Android sparse image.
pub fn image_uncompressed_size(path: &Path) -> Result<u64> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if let Some((blk_sz, total_blks, _, _, _)) = read_sparse_header(&mut f)? {
        return Ok(blk_sz as u64 * total_blks as u64);
    }
    Ok(f.metadata()?.len())
}

/// Write a raw or Android sparse image directly to `dst` starting at `dst_offset`,
/// preserving sparse holes for `DONT_CARE` chunks.
fn write_image_at_offset(src_path: &Path, dst: &mut File, dst_offset: u64) -> Result<u64> {
    let mut src = File::open(src_path).with_context(|| format!("opening {}", src_path.display()))?;
    if let Some((blk_sz, total_blks, total_chunks, file_hdr_sz, chunk_hdr_sz)) =
        read_sparse_header(&mut src)?
    {
        if file_hdr_sz < 28 || chunk_hdr_sz < 12 {
            bail!("{}: invalid sparse header sizes", src_path.display());
        }
        src.seek(SeekFrom::Start(file_hdr_sz as u64))?;
        let mut out_pos = dst_offset;
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        for _ in 0..total_chunks {
            let mut ch = [0u8; 12];
            src.read_exact(&mut ch)?;
            if chunk_hdr_sz > 12 {
                src.seek(SeekFrom::Current((chunk_hdr_sz - 12) as i64))?;
            }
            let chunk_type = u16::from_le_bytes(ch[0..2].try_into().unwrap());
            let chunk_blks = u32::from_le_bytes(ch[4..8].try_into().unwrap()) as u64;
            let total_sz = u32::from_le_bytes(ch[8..12].try_into().unwrap()) as u64;
            let data_sz = total_sz
                .checked_sub(chunk_hdr_sz as u64)
                .context("sparse chunk total_sz < chunk_hdr_sz")?;
            let out_bytes = chunk_blks * blk_sz as u64;

            match chunk_type {
                CHUNK_RAW => {
                    if data_sz != out_bytes {
                        bail!("{}: sparse RAW size mismatch", src_path.display());
                    }
                    dst.seek(SeekFrom::Start(out_pos))?;
                    let mut rem = data_sz;
                    while rem > 0 {
                        let n = (rem as usize).min(buf.len());
                        src.read_exact(&mut buf[..n])?;
                        dst.write_all(&buf[..n])?;
                        rem -= n as u64;
                    }
                    out_pos += out_bytes;
                }
                CHUNK_FILL => {
                    let mut fill = [0u8; 4];
                    src.read_exact(&mut fill)?;
                    if data_sz > 4 {
                        src.seek(SeekFrom::Current((data_sz - 4) as i64))?;
                    }
                    if fill == [0, 0, 0, 0] {
                        // Sparse zero hole — no need to allocate blocks on APFS.
                        out_pos += out_bytes;
                    } else {
                        dst.seek(SeekFrom::Start(out_pos))?;
                        for chunk in buf.chunks_exact_mut(4) {
                            chunk.copy_from_slice(&fill);
                        }
                        let mut rem = out_bytes;
                        while rem > 0 {
                            let n = (rem as usize).min(buf.len());
                            dst.write_all(&buf[..n])?;
                            rem -= n as u64;
                        }
                        out_pos += out_bytes;
                    }
                }
                CHUNK_DONT_CARE => {
                    if data_sz > 0 {
                        src.seek(SeekFrom::Current(data_sz as i64))?;
                    }
                    out_pos += out_bytes;
                }
                CHUNK_CRC32 => {
                    if data_sz > 0 {
                        src.seek(SeekFrom::Current(data_sz as i64))?;
                    }
                }
                other => bail!("{}: unsupported sparse chunk type 0x{other:x}", src_path.display()),
            }
        }
        return Ok(blk_sz as u64 * total_blks as u64);
    }

    // Plain raw image.
    src.seek(SeekFrom::Start(0))?;
    dst.seek(SeekFrom::Start(dst_offset))?;
    let written = std::io::copy(&mut src, dst)?;
    Ok(written)
}

fn round_up_sectors(bytes: u64) -> u64 {
    let sectors = (bytes + SECTOR_SIZE - 1) / SECTOR_SIZE;
    ((sectors + ALIGN_SECTORS - 1) / ALIGN_SECTORS) * ALIGN_SECTORS
}

fn deterministic_guid(seed: &str) -> [u8; 16] {
    let h1 = crc32fast::hash(seed.as_bytes());
    let h2 = crc32fast::hash(format!("{seed}:2").as_bytes());
    let h3 = crc32fast::hash(format!("{seed}:3").as_bytes());
    let h4 = crc32fast::hash(format!("{seed}:4").as_bytes());
    let mut g = [0u8; 16];
    g[0..4].copy_from_slice(&h1.to_le_bytes());
    g[4..8].copy_from_slice(&h2.to_le_bytes());
    g[8..12].copy_from_slice(&h3.to_le_bytes());
    g[12..16].copy_from_slice(&h4.to_le_bytes());
    g[6] = (g[6] & 0x0F) | 0x40; // version 4
    g[8] = (g[8] & 0x3F) | 0x80; // RFC 4122 variant
    g
}

fn build_protective_mbr(total_sectors: u64) -> [u8; 512] {
    let mut mbr = [0u8; 512];
    let entry = &mut mbr[446..462];
    entry[0] = 0x00;
    entry[1] = 0x00;
    entry[2] = 0x02;
    entry[3] = 0x00;
    entry[4] = 0xEE; // GPT Protective
    entry[5] = 0xFF;
    entry[6] = 0xFF;
    entry[7] = 0xFF;
    entry[8..12].copy_from_slice(&1u32.to_le_bytes());
    let size_lba = (total_sectors - 1).min(u32::MAX as u64) as u32;
    entry[12..16].copy_from_slice(&size_lba.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    mbr
}

fn build_gpt_header(
    my_lba: u64,
    alt_lba: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: &[u8; 16],
    entries_lba: u64,
    entries_crc: u32,
) -> [u8; 512] {
    let mut hdr = [0u8; 512];
    hdr[0..8].copy_from_slice(b"EFI PART");
    hdr[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
    hdr[12..16].copy_from_slice(&92u32.to_le_bytes()); // header size
    hdr[24..32].copy_from_slice(&my_lba.to_le_bytes());
    hdr[32..40].copy_from_slice(&alt_lba.to_le_bytes());
    hdr[40..48].copy_from_slice(&first_usable.to_le_bytes());
    hdr[48..56].copy_from_slice(&last_usable.to_le_bytes());
    hdr[56..72].copy_from_slice(disk_guid);
    hdr[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    hdr[80..84].copy_from_slice(&GPT_ENTRIES.to_le_bytes());
    hdr[84..88].copy_from_slice(&GPT_ENTRY_SIZE.to_le_bytes());
    hdr[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let crc = crc32fast::hash(&hdr[0..92]);
    hdr[16..20].copy_from_slice(&crc.to_le_bytes());
    hdr
}

/// Read the partition names from the primary GPT of an existing disk image.
pub fn partition_names(disk_path: &Path) -> Result<Vec<String>> {
    let mut f = File::open(disk_path).with_context(|| format!("opening {}", disk_path.display()))?;
    let mut hdr = [0u8; 512];
    f.seek(SeekFrom::Start(SECTOR_SIZE))?;
    f.read_exact(&mut hdr)?;
    if &hdr[0..8] != b"EFI PART" {
        bail!("{}: no GPT header", disk_path.display());
    }
    let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let count = u32::from_le_bytes(hdr[80..84].try_into().unwrap()).min(1024) as usize;
    let entry_size = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
    if entry_size < 128 {
        bail!("{}: bad GPT entry size {entry_size}", disk_path.display());
    }
    let mut entries = vec![0u8; count * entry_size];
    f.seek(SeekFrom::Start(entries_lba * SECTOR_SIZE))?;
    f.read_exact(&mut entries)?;
    Ok(entries
        .chunks_exact(entry_size)
        .filter(|e| e[0..16].iter().any(|&b| b != 0))
        .map(|e| {
            let units: Vec<u16> = e[56..128]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0)
                .collect();
            String::from_utf16_lossy(&units)
        })
        .collect())
}

/// Create a sparse GPT disk image at `disk_path` from the provided partition specs.
pub fn build_gpt_disk(disk_path: &Path, specs: &[PartitionSpec<'_>]) -> Result<()> {
    if specs.len() > GPT_ENTRIES as usize {
        bail!("too many partitions ({} > {GPT_ENTRIES})", specs.len());
    }
    if let Some(parent) = disk_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Compute layout.
    let first_usable: u64 = ALIGN_SECTORS; // 1 MiB
    let mut cursor = first_usable;
    let mut layout = Vec::with_capacity(specs.len());
    for spec in specs {
        let raw_size = match spec.source {
            PartitionSource::File(p) => image_uncompressed_size(p)?,
            PartitionSource::Empty { size_bytes } => size_bytes,
        };
        let part_bytes = raw_size.max(spec.min_size_bytes).max(ALIGN_BYTES);
        let sectors = round_up_sectors(part_bytes);
        let start_lba = cursor;
        let end_lba = start_lba + sectors - 1;
        layout.push((spec, start_lba, end_lba));
        cursor += sectors;
    }

    let last_usable = cursor - 1;
    let backup_entries_lba = cursor;
    let backup_header_lba = backup_entries_lba + GPT_ENTRIES_SECTORS;
    let total_sectors = backup_header_lba + 1;
    let total_bytes = total_sectors * SECTOR_SIZE;

    let mut disk = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .read(true)
        .open(disk_path)
        .with_context(|| format!("creating {}", disk_path.display()))?;
    disk.set_len(total_bytes)?;

    // Write partition payloads.
    let mut entries_buf = vec![0u8; (GPT_ENTRIES * GPT_ENTRY_SIZE) as usize];
    for (idx, (spec, start_lba, end_lba)) in layout.iter().enumerate() {
        let off = start_lba * SECTOR_SIZE;
        let part_mib = (end_lba - start_lba + 1) * SECTOR_SIZE / (1024 * 1024);
        match spec.source {
            PartitionSource::File(p) => {
                println!("[gpt] {:20} {:>5} MiB  <- {}", spec.name, part_mib, p.display());
                write_image_at_offset(p, &mut disk, off)?;
            }
            PartitionSource::Empty { .. } => {
                println!("[gpt] {:20} {:>5} MiB  (empty/sparse)", spec.name, part_mib);
            }
        }

        let e = &mut entries_buf[idx * 128..(idx + 1) * 128];
        e[0..16].copy_from_slice(&LINUX_FS_TYPE_GUID);
        e[16..32].copy_from_slice(&deterministic_guid(&format!("aosp14:part:{}", spec.name)));
        e[32..40].copy_from_slice(&start_lba.to_le_bytes());
        e[40..48].copy_from_slice(&end_lba.to_le_bytes());
        // UTF-16LE partition name (max 36 UTF-16 code units = 72 bytes).
        for (i, ch) in spec.name.encode_utf16().take(36).enumerate() {
            e[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&ch.to_le_bytes());
        }
    }

    let disk_guid = deterministic_guid("aosp14:disk");
    let entries_crc = crc32fast::hash(&entries_buf);
    let mbr = build_protective_mbr(total_sectors);
    let primary_hdr = build_gpt_header(
        1,
        backup_header_lba,
        first_usable,
        last_usable,
        &disk_guid,
        2,
        entries_crc,
    );
    let backup_hdr = build_gpt_header(
        backup_header_lba,
        1,
        first_usable,
        last_usable,
        &disk_guid,
        backup_entries_lba,
        entries_crc,
    );

    disk.seek(SeekFrom::Start(0))?;
    disk.write_all(&mbr)?;
    disk.write_all(&primary_hdr)?;
    disk.write_all(&entries_buf)?;

    disk.seek(SeekFrom::Start(backup_entries_lba * SECTOR_SIZE))?;
    disk.write_all(&entries_buf)?;
    disk.write_all(&backup_hdr)?;
    disk.sync_all()?;

    println!(
        "[gpt] Wrote {} ({:.2} GiB virtual, {} partitions)",
        disk_path.display(),
        total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        specs.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_valid_gpt_headers() {
        let dir = std::env::temp_dir().join(format!("aosp14-gpt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.img");
        build_gpt_disk(
            &disk,
            &[
                PartitionSpec {
                    name: "misc",
                    source: PartitionSource::Empty { size_bytes: 1024 * 1024 },
                    min_size_bytes: 0,
                },
                PartitionSpec {
                    name: "metadata",
                    source: PartitionSource::Empty { size_bytes: 2 * 1024 * 1024 },
                    min_size_bytes: 0,
                },
            ],
        )
        .unwrap();
        let data = fs::read(&disk).unwrap();
        assert_eq!(&data[510..512], &[0x55, 0xAA]);
        assert_eq!(&data[512..520], b"EFI PART");
        let n = data.len();
        assert_eq!(&data[n - 512..n - 504], b"EFI PART");
        assert_eq!(partition_names(&disk).unwrap(), vec!["misc", "metadata"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
