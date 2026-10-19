// Catalogs contain indexes, not blob payloads. Mobile processes have a much
// smaller virtual address space than desktop processes.
pub(super) const fn default_catalog_map_size_bytes(ios: bool) -> u64 {
    if ios {
        128 * 1024 * 1024
    } else {
        64 * 1024 * 1024 * 1024
    }
}

#[cfg(test)]
mod tests {
    use super::default_catalog_map_size_bytes;

    #[test]
    fn ios_catalog_leaves_address_space_for_blob_stores_and_ui() {
        let bytes = default_catalog_map_size_bytes(true);
        assert!(bytes >= 16 * 1024 * 1024);
        assert!(bytes <= 128 * 1024 * 1024);
    }

    #[test]
    fn desktop_catalog_retains_its_existing_capacity() {
        assert_eq!(
            default_catalog_map_size_bytes(false),
            64 * 1024 * 1024 * 1024
        );
    }
}
