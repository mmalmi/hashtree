use std::fs::Metadata;
use std::io;
use std::os::unix::fs::MetadataExt;

// systemd can recycle an fd number between reads of /proc/<pid>/fd/<n>.
// Retain every file or directory seen, even if its number is reused or closed.
pub(super) fn collect_fd(
    mut inspect: impl FnMut() -> io::Result<Metadata>,
    mut record: impl FnMut(&Metadata),
) -> io::Result<()> {
    let mut previous = None;
    for _ in 0..4 {
        let metadata = match inspect() {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let identity = metadata.as_ref().map(|m| (m.dev(), m.ino()));
        if previous == Some(identity) {
            return Ok(());
        }
        match metadata {
            Some(m) if m.is_file() || m.is_dir() => record(&m),
            _ if previous.is_none() => return Ok(()),
            _ => {}
        }
        previous = Some(identity);
    }
    Err(io::Error::other("descriptor identity did not stabilize"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::path::Path;

    fn run_sequence(paths: &[Option<&Path>]) -> (io::Result<()>, Vec<u64>) {
        let mut paths = paths.iter();
        let mut seen = Vec::new();
        let result = collect_fd(
            || match paths.next().expect("unexpected extra sample") {
                Some(path) => File::open(path)?.metadata(),
                None => Err(io::Error::from(io::ErrorKind::NotFound)),
            },
            |metadata| seen.push(metadata.ino()),
        );
        (result, seen)
    }

    #[test]
    fn collects_both_holders_when_a_descriptor_is_reused() {
        let exe = std::env::current_exe().unwrap();
        let a = Some(exe.as_path());
        let b = Some(Path::new("."));
        let (result, seen) = run_sequence(&[a, b, b]);
        result.unwrap();
        assert_eq!(
            seen,
            [
                exe.metadata().unwrap().ino(),
                Path::new(".").metadata().unwrap().ino()
            ]
        );
    }

    #[test]
    fn tracks_a_closed_descriptor_and_checks_its_replacement() {
        let exe = std::env::current_exe().unwrap();
        let a = Some(exe.as_path());
        let b = Some(Path::new("."));
        let (result, seen) = run_sequence(&[a, None, b, b]);
        result.unwrap();
        assert_eq!(
            seen,
            [
                exe.metadata().unwrap().ino(),
                Path::new(".").metadata().unwrap().ino()
            ]
        );
        let (result, seen) = run_sequence(&[a, None, None]);
        result.unwrap();
        assert_eq!(seen, [exe.metadata().unwrap().ino()]);
    }

    #[test]
    fn persistent_descriptor_reuse_fails_closed() {
        let exe = std::env::current_exe().unwrap();
        let a = Some(exe.as_path());
        let b = Some(Path::new("."));
        let (result, _) = run_sequence(&[a, b, a, b]);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
    }

    #[test]
    fn inspection_errors_are_returned() {
        let err = collect_fd(
            || Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            |_| unreachable!(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }
}
