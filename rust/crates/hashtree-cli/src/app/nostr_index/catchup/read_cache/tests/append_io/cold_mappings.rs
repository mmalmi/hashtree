//! Select only the exact owned, read-only shared file mapping. Never use paths
//! or an address supplied by the environment to select memory for madvise.
#[derive(Debug, PartialEq)]
struct Mapping {
    start: usize,
    bytes: usize,
}

fn select(
    maps: &str,
    device: (u32, u32),
    inode: u64,
    file_bytes: usize,
    page: usize,
) -> Result<Mapping, &'static str> {
    if inode == 0 || page == 0 || !page.is_power_of_two() || file_bytes == 0 {
        return Err("invalid owned file or page size");
    }
    let bytes = file_bytes.checked_add(page - 1).ok_or("length overflow")? / page * page;
    let mut selected = None;
    for line in maps.lines() {
        let fields: Vec<_> = line.split_whitespace().take(5).collect();
        if fields.len() != 5 {
            return Err("malformed mapping");
        }
        let (major, minor) = fields[3].split_once(':').ok_or("malformed device")?;
        let found = (
            u32::from_str_radix(major, 16).map_err(|_| "invalid major")?,
            u32::from_str_radix(minor, 16).map_err(|_| "invalid minor")?,
        );
        let found_inode = fields[4].parse::<u64>().map_err(|_| "invalid inode")?;
        if found != device || found_inode != inode {
            continue;
        }
        if selected.is_some() {
            return Err("multiple mappings of owned file");
        }
        if fields[1] != "r--s" {
            return Err("owned mapping must be read-only shared");
        }
        if usize::from_str_radix(fields[2], 16).map_err(|_| "invalid offset")? != 0 {
            return Err("owned mapping must begin at file offset zero");
        }
        let (start, end) = fields[0].split_once('-').ok_or("malformed range")?;
        let start = usize::from_str_radix(start, 16).map_err(|_| "invalid start")?;
        let end = usize::from_str_radix(end, 16).map_err(|_| "invalid end")?;
        let length = end.checked_sub(start).ok_or("reversed mapping")?;
        if start == 0
            || start % page != 0
            || end % page != 0
            || length < bytes
            || length > 2 * 1024 * 1024 * 1024
        {
            return Err("unaligned or unbounded owned mapping");
        }
        selected = Some(Mapping { start, bytes });
    }
    selected.ok_or("owned file mapping absent")
}

#[cfg(target_os = "linux")]
pub(super) unsafe fn discard_resident_pages(file: &std::fs::File) -> usize {
    use std::os::unix::fs::MetadataExt;
    // Caller owns a quiescent disposable Pool, with no active transactions,
    // workers, mapping resize/close, or other users of these files.
    let metadata = file.metadata().unwrap();
    let page: usize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }
        .try_into()
        .unwrap();
    let device = (libc::major(metadata.dev()), libc::minor(metadata.dev()));
    let mapping = select(
        &std::fs::read_to_string("/proc/self/maps").unwrap(),
        device,
        metadata.ino(),
        metadata.len().try_into().unwrap(),
        page,
    )
    .unwrap();
    // SAFETY: the validated range is a read-only MAP_SHARED view of this exact
    // already-synced file. MADV_DONTNEED preserves backing bytes and the virtual
    // mapping; subsequent LMDB access faults the same bytes back in.
    assert_eq!(
        unsafe {
            libc::madvise(
                mapping.start as *mut libc::c_void,
                mapping.bytes,
                libc::MADV_DONTNEED,
            )
        },
        0,
        "owned mapping discard failed"
    );
    mapping.bytes
}

#[test]
fn mapping_selection_requires_exact_device_inode_and_bounds() {
    let maps = "1000-9000 r--s 00000000 08:01 42 /temporary/catalog/data.mdb\n9000-b000 rw-s 00000000 08:01 43 /temporary/catalog/lock.mdb\nb000-c000 r--s 00000000 08:02 42 /unrelated/data.mdb\nc000-d000 r--p 00000000 00:00 0\n";
    assert_eq!(
        select(maps, (8, 1), 42, 8193, 4096),
        Ok(Mapping {
            start: 4096,
            bytes: 12288
        })
    );
    assert!(select(maps, (8, 1), 99, 8193, 4096).is_err());
    assert!(select(maps, (8, 1), 42, 65536, 4096).is_err());
    assert!(select(maps, (8, 1), 0, 8193, 4096).is_err());
}

#[test]
fn mapping_selection_rejects_unsafe_or_ambiguous_owned_views() {
    let valid = "1000-9000 r--s 00000000 08:01 42 /temporary/data.mdb\n";
    for invalid in [
        valid.replace("r--s", "rw-s"),
        valid.replace("r--s", "r--p"),
        valid.replace("00000000", "00001000"),
        valid.replace("1000-9000", "1001-9000"),
        valid.replace("1000-9000", "9000-1000"),
        format!("{valid}{valid}"),
    ] {
        assert!(select(&invalid, (8, 1), 42, 8193, 4096).is_err());
    }
}
