/// The HT file.
///
/// The file that stores the hash-table buckets and the meta map.
use super::meta_map::MetaMap;
use crate::io::{Page, PAGE_SIZE};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek},
    path::PathBuf,
};

/// The offsets of the HT file.
#[derive(Clone)]
pub struct HTOffsets {
    // the number of pages to add to a page number to find its real location in the file,
    // taking account of the meta page and meta byte pages.
    data_page_offset: u64,
}

impl HTOffsets {
    /// Returns the page number of the `ix`th item in the data section of the store.
    pub fn data_page_index(&self, ix: u64) -> u64 {
        self.data_page_offset + ix
    }

    /// Returns the page number of the `ix`th item in the meta bytes section of the store.
    pub fn meta_bytes_index(&self, ix: u64) -> u64 {
        ix
    }
}

fn expected_file_len(num_pages: u32) -> u64 {
    (num_meta_byte_pages(num_pages) + num_pages) as u64 * PAGE_SIZE as u64
}

fn num_meta_byte_pages(num_pages: u32) -> u32 {
    (num_pages + 4095) / PAGE_SIZE as u32
}

/// Opens the HT file, checks its length and reads the meta map.
pub fn open(num_pages: u32, mut ht_fd: &File) -> anyhow::Result<(HTOffsets, MetaMap)> {
    if ht_fd.metadata()?.len() != expected_file_len(num_pages) {
        anyhow::bail!("Store corrupted; unexpected file length");
    }

    // Read the extra meta pages. Note that due to O_DIRECT we are only allowed to read into
    // aligned buffers. You cannot really conjure a Vec from raw parts because the Vec doesn't
    // store alignment but deducts it from T before deallocation and the allocator might not
    // like that.
    //
    // We could try to be smart about this sure, but there is always a risk to outsmart yourself
    // pooping your own pants on the way.
    ht_fd.seek(std::io::SeekFrom::Start(0))?;
    let num_meta_byte_pages = num_meta_byte_pages(num_pages) as usize;
    let mut extra_meta_pages: Vec<Page> = Vec::with_capacity(num_meta_byte_pages);
    for _ in 0..num_meta_byte_pages {
        let mut buf = Page::zeroed();
        ht_fd.read_exact(&mut buf)?;
        extra_meta_pages.push(buf);
    }
    let mut meta_bytes = Vec::with_capacity(num_meta_byte_pages * PAGE_SIZE);
    for extra_meta_page in extra_meta_pages {
        meta_bytes.extend_from_slice(&*extra_meta_page);
    }

    let data_page_offset = num_meta_byte_pages as u64;
    Ok((
        HTOffsets { data_page_offset },
        MetaMap::from_bytes(meta_bytes, num_pages as usize),
    ))
}

/// Creates the store file. Fails if store file already exists.
///
/// Lays out the meta page, and fills the file with zeroes.
pub fn create(path: PathBuf, num_pages: u32) -> std::io::Result<()> {
    let start = std::time::Instant::now();
    let ht_path = path.join("ht");
    let ht_file = OpenOptions::new().write(true).create(true).open(ht_path)?;

    // number of pages + pages required for meta bits.
    let page_count = num_pages + num_meta_byte_pages(num_pages);
    let len = page_count as usize * PAGE_SIZE;
    ht_file.set_len(len as u64)?;
    ht_file.sync_all()?;
    drop(ht_file);

    let wal_path = path.join("wal");
    let wal_file = OpenOptions::new().write(true).create(true).open(wal_path)?;
    wal_file.sync_all()?;
    drop(wal_file);

    println!(
        "Created file with {} total pages in {}ms",
        page_count,
        start.elapsed().as_millis()
    );
    Ok(())
}
