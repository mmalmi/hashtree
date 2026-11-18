use std::fs::Metadata;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

// systemd can recycle an fd number between reads of /proc/<pid>/fd/<n>.
// Retain every file or directory seen, even if its number is reused or closed.
pub(super) fn collect_fd(
    inspect: impl FnMut() -> io::Result<Metadata>,
    record: impl FnMut(&Metadata),
) -> io::Result<()> {
    collect_fd_with_wait(inspect, record, std::thread::sleep)
}

fn collect_fd_with_wait(
    mut inspect: impl FnMut() -> io::Result<Metadata>,
    mut record: impl FnMut(&Metadata),
    mut wait: impl FnMut(Duration),
) -> io::Result<()> {
    let mut previous = None;
    // Four immediate observations preserve the common path. A single bounded
    // resampling window permits finite manager churn to settle, without ever
    // accepting a persistently ambiguous slot. Maximum requested sleep: 15 ms.
    for sample in 0..8 {
        if sample >= 4 {
            wait(Duration::from_millis(1 << (sample - 4)));
        }
        let metadata = match inspect() {
            Ok(metadata) => Some(metadata),
            // An initially absent fd has no observed holder to reconcile.
            Err(error) if error.kind() == io::ErrorKind::NotFound && previous.is_none() => {
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        // Sockets, pipes and devices are not migration file/directory
        // authorities. Two such observations establish absence even when their
        // unrelated inode numbers differ. Never discard an observed file/dir.
        let metadata = metadata.filter(|m| m.is_file() || m.is_dir());
        let identity = metadata.as_ref().map(|m| (m.dev(), m.ino()));
        if identity.is_some_and(|(device, inode)| device == 0 || inode == 0) {
            return Err(io::Error::other(
                "descriptor has an unknown device/inode identity",
            ));
        }
        if previous == Some(identity) {
            return Ok(());
        }
        if let Some(metadata) = metadata {
            record(&metadata);
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
        let (result, seen, _) = run_sequence_with_wait(paths);
        (result, seen)
    }

    fn run_sequence_with_wait(
        paths: &[Option<&Path>],
    ) -> (io::Result<()>, Vec<u64>, Vec<Duration>) {
        let mut paths = paths.iter();
        let mut seen = Vec::new();
        let mut waits = Vec::new();
        let result = collect_fd_with_wait(
            || match paths.next().expect("unexpected extra sample") {
                Some(path) => File::open(path)?.metadata(),
                None => Err(io::Error::from(io::ErrorKind::NotFound)),
            },
            |metadata| seen.push(metadata.ino()),
            |duration| waits.push(duration),
        );
        (result, seen, waits)
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
        let (result, seen, waits) = run_sequence_with_wait(&[a, b, a, b, a, b, a, b]);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(seen.len(), 8);
        assert_eq!(waits, [1, 2, 4, 8].map(Duration::from_millis));
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
    #[test]
    fn settles_after_four_initial_samples() {
        let exe = std::env::current_exe().unwrap();
        let a = Some(exe.as_path());
        let b = Some(Path::new("."));
        let (result, seen, waits) = run_sequence_with_wait(&[a, b, a, b, a, a]);
        result.unwrap();
        let a_inode = exe.metadata().unwrap().ino();
        let b_inode = Path::new(".").metadata().unwrap().ino();
        assert_eq!(seen, [a_inode, b_inode, a_inode, b_inode, a_inode]);
        assert_eq!(waits, [1, 2].map(Duration::from_millis));
    }

    #[test]
    fn non_file_observations_require_two_samples() {
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        let (socket, _peer) = UnixStream::pair().unwrap();
        let socket = File::from(OwnedFd::from(socket));
        let device = File::open("/dev/null").unwrap();
        let mut samples = 0;
        collect_fd(
            || {
                samples += 1;
                if samples == 1 {
                    socket.metadata()
                } else {
                    device.metadata()
                }
            },
            |_| panic!("special descriptors are not file/directory authorities"),
        )
        .unwrap();
        assert_eq!(samples, 2);
    }

    #[test]
    fn stable_files_and_initial_absence_never_wait() {
        let exe = std::env::current_exe().unwrap();
        let a = Some(exe.as_path());
        let (result, seen, waits) = run_sequence_with_wait(&[a, a]);
        result.unwrap();
        assert_eq!(seen, [exe.metadata().unwrap().ino()]);
        assert!(waits.is_empty());
        let (result, seen, waits) = run_sequence_with_wait(&[None]);
        result.unwrap();
        assert!(seen.is_empty());
        assert!(waits.is_empty());
    }

    #[test]
    fn observed_target_is_retained_when_unrelated_special_handles_settle() {
        use std::os::fd::OwnedFd;
        use std::os::unix::net::UnixStream;
        let target = File::open(std::env::current_exe().unwrap()).unwrap();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let socket = File::from(OwnedFd::from(socket));
        let device = File::open("/dev/null").unwrap();
        let mut files = [&target, &socket, &device].into_iter();
        let mut seen = Vec::new();
        collect_fd_with_wait(
            || files.next().expect("unexpected extra sample").metadata(),
            |m| seen.push((m.dev(), m.ino())),
            |_| panic!("settled in the immediate window"),
        )
        .unwrap();
        let target = target.metadata().unwrap();
        // The unchanged caller rejects this retained authority against its
        // scanned inventory, even though the numeric fd now has another use.
        assert_eq!(seen, [(target.dev(), target.ino())]);
    }

    #[test]
    fn persistent_file_and_absence_churn_fails_closed() {
        let exe = std::env::current_exe().unwrap();
        let a = Some(exe.as_path());
        let (result, seen, waits) = run_sequence_with_wait(&[a, None, a, None, a, None, a, None]);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(seen, [exe.metadata().unwrap().ino(); 4]);
        assert_eq!(waits, [1, 2, 4, 8].map(Duration::from_millis));
    }

    #[test]
    fn resampling_permission_errors_fail_closed_and_keep_observed_holders() {
        let file = File::open(std::env::current_exe().unwrap()).unwrap();
        let directory = File::open(".").unwrap();
        let mut samples = 0;
        let mut seen = Vec::new();
        let mut waits = Vec::new();
        let error = collect_fd_with_wait(
            || {
                samples += 1;
                if samples == 5 {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else if samples % 2 == 0 {
                    directory.metadata()
                } else {
                    file.metadata()
                }
            },
            |m| seen.push(m.ino()),
            |duration| waits.push(duration),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(samples, 5);
        assert_eq!(seen.len(), 4);
        assert_eq!(waits, [Duration::from_millis(1)]);
    }
}
