use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

const MARKERS: [&str; 3] = ["SingletonLock", "SingletonCookie", "SingletonSocket"];
const MAX_SCAN_DEPTH: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaleProfileLock {
    pub profile_dir: PathBuf,
    pub pid: u32,
    pub markers: Vec<String>,
}

impl StaleProfileLock {
    pub fn profile_name(&self) -> String {
        self.profile_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.profile_dir.display().to_string())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error("the profile lock changed; scan again before cleaning it")]
    Changed,
    #[error("the profile lock is active again; it was not changed")]
    Active,
    #[error("the lock entries are not safe symlinks; nothing was removed")]
    Unsafe,
    #[error("could not remove the stale profile lock: {0}")]
    Io(#[from] io::Error),
}

pub fn scan() -> io::Result<Vec<StaleProfileLock>> {
    let Some(root) = config_root() else {
        return Ok(Vec::new());
    };
    scan_roots(std::slice::from_ref(&root))
}

pub fn cleanup(lock: &StaleProfileLock) -> Result<usize, CleanupError> {
    let Some(current) = inspect_profile(&lock.profile_dir)? else {
        return Err(CleanupError::Changed);
    };
    if current.pid != lock.pid {
        return Err(CleanupError::Changed);
    }
    if process_exists(current.pid) {
        return Err(CleanupError::Active);
    }
    if current.markers != lock.markers {
        return Err(CleanupError::Changed);
    }
    if current
        .markers
        .iter()
        .any(|marker| !is_symlink(&lock.profile_dir.join(marker)))
    {
        return Err(CleanupError::Unsafe);
    }

    let mut removed = 0;
    for marker in current.markers {
        let path = lock.profile_dir.join(marker);
        match fs::remove_file(path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(CleanupError::Io(error)),
        }
    }
    Ok(removed)
}

fn config_root() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

fn scan_roots(roots: &[PathBuf]) -> io::Result<Vec<StaleProfileLock>> {
    let mut locks = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        scan_directory(root, 0, &mut locks)?;
    }
    locks.sort_by(|first, second| first.profile_dir.cmp(&second.profile_dir));
    locks.dedup_by(|first, second| first.profile_dir == second.profile_dir);
    Ok(locks)
}

fn scan_directory(
    directory: &Path,
    depth: usize,
    locks: &mut Vec<StaleProfileLock>,
) -> io::Result<()> {
    if let Some(lock) = inspect_profile(directory)? {
        locks.push(lock);
    }
    if depth >= MAX_SCAN_DEPTH {
        return Ok(());
    }

    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            scan_directory(&path, depth + 1, locks)?;
        }
    }
    Ok(())
}

fn inspect_profile(directory: &Path) -> io::Result<Option<StaleProfileLock>> {
    let lock_path = directory.join(MARKERS[0]);
    let metadata = match fs::symlink_metadata(&lock_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(None);
    }
    let target = fs::read_link(lock_path)?;
    let Some(pid) = pid_from_lock_target(&target) else {
        return Ok(None);
    };
    if process_exists(pid) {
        return Ok(None);
    }

    let mut markers = MARKERS
        .iter()
        .filter(|marker| is_symlink(&directory.join(marker)))
        .map(|marker| (*marker).to_owned())
        .collect::<Vec<_>>();
    markers.sort();
    Ok(Some(StaleProfileLock {
        profile_dir: directory.to_owned(),
        pid,
        markers,
    }))
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn pid_from_lock_target(target: &Path) -> Option<u32> {
    let name = target.file_name()?.to_string_lossy();
    let (_, pid) = name.rsplit_once('-')?;
    let pid = pid.parse::<u32>().ok()?;
    (pid > 0).then_some(pid)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::symlink,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = env::temp_dir().join(format!("duck-packages-locks-{stamp}"));
            fs::create_dir_all(&root).expect("fixture root");
            Self { root }
        }

        fn profile(&self) -> PathBuf {
            let profile = self.root.join("Codex");
            fs::create_dir_all(&profile).expect("profile");
            profile
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn finds_only_stale_singleton_symlinks() {
        let fixture = Fixture::new();
        let profile = fixture.profile();
        for marker in MARKERS {
            symlink(
                if marker == "SingletonLock" {
                    "localhost-4294967294"
                } else {
                    "/tmp/duck-packages-test"
                },
                profile.join(marker),
            )
            .expect("marker");
        }
        let locks = scan_roots(std::slice::from_ref(&fixture.root)).expect("scan");
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].pid, 4_294_967_294);
        assert_eq!(locks[0].markers.len(), 3);
    }

    #[test]
    fn ignores_regular_lock_files() {
        let fixture = Fixture::new();
        let profile = fixture.profile();
        fs::write(profile.join("SingletonLock"), "localhost-4294967294").expect("lock");
        assert!(
            scan_roots(std::slice::from_ref(&fixture.root))
                .expect("scan")
                .is_empty()
        );
    }

    #[test]
    fn ignores_a_lock_whose_pid_is_still_active() {
        let fixture = Fixture::new();
        let profile = fixture.profile();
        symlink(
            format!("localhost-{}", std::process::id()),
            profile.join("SingletonLock"),
        )
        .expect("lock");
        assert!(
            scan_roots(std::slice::from_ref(&fixture.root))
                .expect("scan")
                .is_empty()
        );
    }

    #[test]
    fn cleanup_removes_only_the_exact_symlinks() {
        let fixture = Fixture::new();
        let profile = fixture.profile();
        for marker in MARKERS {
            symlink(
                if marker == "SingletonLock" {
                    "localhost-4294967294"
                } else {
                    "/tmp/duck-packages-test"
                },
                profile.join(marker),
            )
            .expect("marker");
        }
        fs::write(profile.join("Bookmarks"), "keep").expect("profile data");
        let lock = scan_roots(std::slice::from_ref(&fixture.root))
            .expect("scan")
            .pop()
            .expect("stale lock");
        assert_eq!(cleanup(&lock).expect("cleanup"), 3);
        assert!(profile.join("Bookmarks").exists());
        assert!(!profile.join("SingletonLock").exists());
        assert!(!profile.join("SingletonCookie").exists());
        assert!(!profile.join("SingletonSocket").exists());
    }

    #[test]
    fn parses_pid_from_the_last_dash_component() {
        assert_eq!(
            pid_from_lock_target(Path::new("host-name-18956")),
            Some(18956)
        );
        assert_eq!(pid_from_lock_target(Path::new("host-name")), None);
        assert_eq!(pid_from_lock_target(Path::new("host-name-0")), None);
    }
}
