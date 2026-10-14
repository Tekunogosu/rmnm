//! The game manifest: every file of a build, its XXH3-128 hash and the zstd chunks it is
//! assembled from. Chunks live inside "bundles", files named by their CRC-64/REDIS.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// Path relative to the install folder, with `/` separators and no leading slash.
    pub path: String,
    /// XXH3-128 of the whole file.
    pub hash: u128,
    pub size: u64,
    /// Sorted by `offset`; together they cover the file exactly.
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u64,
    pub length: u64,
    /// CRC-64/REDIS of the decompressed chunk.
    pub crc: u64,
    pub bundle: u64,
    pub bundle_offset: u64,
    pub length_in_bundle: u64,
}

impl FileEntry {
    pub fn local_path(&self, install_dir: &Path) -> PathBuf {
        install_dir.join(&self.path)
    }
}

/// A bundle's file name on the chunk server, as the manifest spells its CRC (no zero padding).
pub fn bundle_file_name(crc: u64) -> String {
    format!("{crc:x}.bin")
}

pub fn parse(json: &[u8]) -> Result<Manifest> {
    #[derive(Deserialize)]
    struct Raw {
        manifest: Vec<RawFile>,
    }
    #[derive(Deserialize)]
    struct RawFile {
        path: String,
        file_hash: String,
        chunks: Vec<RawChunk>,
    }
    #[derive(Deserialize)]
    struct RawChunk {
        offset: u64,
        length: u64,
        crc: String,
        bundle_offset: u64,
        length_in_bundle: u64,
        bundle_crc: String,
    }

    let raw: Raw = serde_json::from_slice(json).context("manifest is not valid JSON")?;
    let files = raw
        .manifest
        .into_iter()
        .map(|file| {
            let path = relative_path(&file.path)?;
            let hash = u128::from_str_radix(&file.file_hash, 16)
                .with_context(|| format!("bad file hash for {path}"))?;
            let mut chunks = file
                .chunks
                .into_iter()
                .map(|c| {
                    Ok(Chunk {
                        offset: c.offset,
                        length: c.length,
                        crc: parse_crc(&c.crc)?,
                        bundle: parse_crc(&c.bundle_crc)?,
                        bundle_offset: c.bundle_offset,
                        length_in_bundle: c.length_in_bundle,
                    })
                })
                .collect::<Result<Vec<_>>>()
                .with_context(|| format!("bad chunk list for {path}"))?;
            chunks.sort_by_key(|c| c.offset);
            let mut size = 0;
            for chunk in &chunks {
                if chunk.offset != size {
                    bail!("chunks of {path} do not cover the file contiguously");
                }
                size += chunk.length;
            }
            Ok(FileEntry {
                path,
                hash,
                size,
                chunks,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Manifest { files })
}

fn parse_crc(hex: &str) -> Result<u64> {
    u64::from_str_radix(hex, 16).map_err(|_| anyhow!("bad CRC {hex:?}"))
}

/// Turns a manifest path (`/mnm_Data/x.bundle`) into a safe relative path. The manifest comes
/// from the network, so anything that could leave the install folder is rejected.
pub fn relative_path(path: &str) -> Result<String> {
    let relative = path.strip_prefix('/').unwrap_or(path);
    let ok = !relative.contains('\\')
        && !relative.contains('\0')
        && relative
            .split('/')
            .all(|seg| !matches!(seg, "" | "." | ".."))
        && Path::new(relative)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if !ok {
        bail!("refusing unsafe manifest path {path:?}");
    }
    Ok(relative.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"manifest":[
        {"path":"/mnm_Data/StreamingAssets/aa/catalog.hash","file_hash":"002de3f29cdb011bbc3474995a1983e8",
         "chunks":[{"offset":0,"length":32,"crc":"9016026b3dec6b34","bundle_offset":0,"length_in_bundle":41,"bundle_crc":"c6b4091d130a20ff"}]},
        {"path":"/two.bin","file_hash":"0000000000000000000000000000000f",
         "chunks":[{"offset":5,"length":7,"crc":"1","bundle_offset":9,"length_in_bundle":4,"bundle_crc":"ab"},
                   {"offset":0,"length":5,"crc":"2","bundle_offset":0,"length_in_bundle":9,"bundle_crc":"ab"}]}
    ]}"#;

    #[test]
    fn parses_real_shape_and_sorts_chunks() {
        let manifest = parse(SAMPLE.as_bytes()).unwrap();
        let first = &manifest.files[0];
        assert_eq!(first.path, "mnm_Data/StreamingAssets/aa/catalog.hash");
        assert_eq!(first.hash, 0x002de3f29cdb011bbc3474995a1983e8);
        assert_eq!(first.size, 32);
        assert_eq!(first.chunks[0].bundle, 0xc6b4091d130a20ff);
        let second = &manifest.files[1];
        assert_eq!(second.size, 12);
        assert_eq!(second.chunks[0].offset, 0);
        assert_eq!(bundle_file_name(second.chunks[0].bundle), "ab.bin");
    }

    #[test]
    fn rejects_paths_outside_the_install() {
        for bad in ["/../etc/passwd", "/a/../../b", "/", "/a\\b", "/a/./b"] {
            assert!(relative_path(bad).is_err(), "{bad} should be rejected");
        }
        assert_eq!(relative_path("/mnm.exe").unwrap(), "mnm.exe");
    }

    #[test]
    fn rejects_gaps_between_chunks() {
        let gap = r#"{"manifest":[{"path":"/f","file_hash":"1","chunks":[
            {"offset":0,"length":4,"crc":"1","bundle_offset":0,"length_in_bundle":4,"bundle_crc":"1"},
            {"offset":5,"length":4,"crc":"1","bundle_offset":4,"length_in_bundle":4,"bundle_crc":"1"}]}]}"#;
        assert!(parse(gap.as_bytes()).is_err());
    }
}
