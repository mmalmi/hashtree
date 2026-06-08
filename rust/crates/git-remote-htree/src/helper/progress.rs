pub(super) struct UploadProgress {
    pub(super) processed: usize,
    pub(super) discovered: usize,
    pub(super) total: Option<usize>,
    pub(super) uploaded: usize,
    pub(super) skipped_diff: usize,
    pub(super) skipped_server: usize,
    pub(super) failed: usize,
    pub(super) has_old_tree: bool,
}

impl UploadProgress {
    fn format_counts(&self) -> String {
        let mut parts = Vec::with_capacity(5);
        if self.total.is_none() {
            parts.push(format!("{} discovered", self.discovered));
        }
        parts.push(format!("{} new", self.uploaded));
        if self.has_old_tree {
            parts.push(format!("{} unchanged", self.skipped_diff));
        }
        parts.push(format!("{} exist", self.skipped_server));
        if self.failed > 0 {
            parts.push(format!("{} FAILED", self.failed));
        }
        parts.join(", ")
    }

    pub(super) fn format(&self) -> String {
        let total = self
            .total
            .map(|value| value.max(self.processed).to_string())
            .unwrap_or_else(|| "?".to_string());
        format!(
            "  Uploading: {}/{} ({})",
            self.processed,
            total,
            self.format_counts()
        )
    }
}

pub(super) fn emit_upload_progress(progress: UploadProgress) {
    eprintln!("{}", progress.format());
}

#[cfg(test)]
mod tests {
    use super::UploadProgress;

    #[test]
    fn upload_progress_formats_known_total_for_old_tree() {
        assert_eq!(
            UploadProgress {
                processed: 12,
                discovered: 34,
                total: Some(34),
                uploaded: 7,
                skipped_diff: 5,
                skipped_server: 0,
                failed: 0,
                has_old_tree: true,
            }
            .format(),
            "  Uploading: 12/34 (7 new, 5 unchanged, 0 exist)"
        );
    }

    #[test]
    fn upload_progress_formats_discovery_state_for_old_tree() {
        assert_eq!(
            UploadProgress {
                processed: 12,
                discovered: 34,
                total: None,
                uploaded: 7,
                skipped_diff: 5,
                skipped_server: 0,
                failed: 0,
                has_old_tree: true,
            }
            .format(),
            "  Uploading: 12/? (34 discovered, 7 new, 5 unchanged, 0 exist)"
        );
    }

    #[test]
    fn upload_progress_formats_failures_for_new_tree() {
        assert_eq!(
            UploadProgress {
                processed: 12,
                discovered: 34,
                total: Some(34),
                uploaded: 7,
                skipped_diff: 0,
                skipped_server: 5,
                failed: 2,
                has_old_tree: false,
            }
            .format(),
            "  Uploading: 12/34 (7 new, 5 exist, 2 FAILED)"
        );
    }

    #[test]
    fn upload_progress_formats_discovery_state_for_new_tree_failures() {
        assert_eq!(
            UploadProgress {
                processed: 12,
                discovered: 34,
                total: None,
                uploaded: 7,
                skipped_diff: 0,
                skipped_server: 5,
                failed: 2,
                has_old_tree: false,
            }
            .format(),
            "  Uploading: 12/? (34 discovered, 7 new, 5 exist, 2 FAILED)"
        );
    }
}
