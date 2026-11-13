//! The total the daemon has spent on retrieval, kept across restarts.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;

use hashtree_paid_retrieval::SpendStore;

/// One number in one file, replaced atomically before each payment.
pub struct FileSpendStore {
    path: PathBuf,
}

impl FileSpendStore {
    /// `path` is the file; its directory must exist.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The saved total, or zero when nothing was spent yet. A file that
    /// cannot be read as a number is an error: guessing zero would hand the
    /// whole budget back.
    pub fn load(&self) -> io::Result<u64> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        text.trim().parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the spending record is not a number",
            )
        })
    }

    fn write(&self, spent_sat: u64) -> io::Result<()> {
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let mut file = File::create(&temporary)?;
        file.write_all(format!("{spent_sat}\n").as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        #[cfg(unix)]
        if let Some(dir) = self.path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

impl SpendStore for FileSpendStore {
    fn save_spent(&self, spent_sat: u64) -> Result<(), String> {
        self.write(spent_sat).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spent_total_survives_a_restart_and_damage_is_not_read_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spent");
        assert_eq!(FileSpendStore::new(&path).load().unwrap(), 0);

        FileSpendStore::new(&path).save_spent(42).unwrap();
        assert_eq!(FileSpendStore::new(&path).load().unwrap(), 42);

        fs::write(&path, "forty-two").unwrap();
        let error = FileSpendStore::new(&path).load().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
