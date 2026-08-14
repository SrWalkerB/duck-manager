use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use rustix::process::{Pid, PidfdFlags, Signal, geteuid, pidfd_open, pidfd_send_signal};

use crate::domain::InstalledApplication;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time_ticks: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessRole {
    Main,
    Child,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessNode {
    pub identity: ProcessIdentity,
    pub executable: String,
    pub role: ProcessRole,
    pub children: Vec<ProcessNode>,
}

impl ProcessNode {
    pub fn process_count(&self) -> usize {
        1 + self
            .children
            .iter()
            .map(ProcessNode::process_count)
            .sum::<usize>()
    }

    pub fn identities(&self, output: &mut Vec<ProcessIdentity>) {
        output.push(self.identity);
        for child in &self.children {
            child.identities(output);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationSession {
    pub root: ProcessNode,
}

impl ApplicationSession {
    pub fn process_count(&self) -> usize {
        self.root.process_count()
    }

    pub fn identities(&self) -> Vec<ProcessIdentity> {
        let mut identities = Vec::new();
        self.root.identities(&mut identities);
        identities
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PossibleProcess {
    pub identity: ProcessIdentity,
    pub executable: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ApplicationProcessState {
    pub sessions: Vec<ApplicationSession>,
    pub possible: Vec<PossibleProcess>,
}

impl ApplicationProcessState {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.possible.is_empty()
    }

    pub fn contains_verified(&self, identity: ProcessIdentity) -> bool {
        self.sessions
            .iter()
            .any(|session| session.identities().contains(&identity))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminationMode {
    Graceful,
    Force,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminationReport {
    pub signaled: usize,
    pub failed: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferencedProcess {
    Missing,
    Verified(ProcessIdentity),
    Possible(ProcessIdentity),
    Unrelated(ProcessIdentity),
    Unreadable,
}

#[derive(Clone, Debug)]
struct ProcessRecord {
    identity: ProcessIdentity,
    parent_pid: u32,
    uid: u32,
    executable_path: Option<PathBuf>,
    argv0: Option<PathBuf>,
    command_name: String,
    cgroup: String,
}

impl ProcessRecord {
    fn display_executable(&self) -> String {
        self.executable_path
            .as_ref()
            .or(self.argv0.as_ref())
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.command_name.clone())
    }
}

#[derive(Clone, Debug)]
struct ApplicationIdentity {
    executable_token: Option<PathBuf>,
    resolved_executable: Option<PathBuf>,
    executable_name: String,
    startup_class: String,
    desktop_keys: Vec<String>,
}

impl ApplicationIdentity {
    fn from_application(application: &InstalledApplication) -> Self {
        let executable_token = application.executable.as_deref().map(PathBuf::from);
        let resolved_executable = executable_token
            .as_deref()
            .and_then(resolve_executable)
            .and_then(|path| canonical_or_original(&path));
        let executable_name = executable_token
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let startup_class = application
            .startup_wm_class
            .as_deref()
            .unwrap_or_default()
            .to_lowercase();
        let desktop_keys = std::iter::once(application.desktop_id.as_str())
            .chain(application.related_desktop_ids.iter().map(String::as_str))
            .map(|id| id.strip_suffix(".desktop").unwrap_or(id).to_lowercase())
            .filter(|id| !id.is_empty())
            .collect();
        Self {
            executable_token,
            resolved_executable,
            executable_name,
            startup_class,
            desktop_keys,
        }
    }

    fn verified_match(&self, record: &ProcessRecord) -> bool {
        record
            .executable_path
            .as_deref()
            .is_some_and(|path| self.path_matches(path))
            || record
                .argv0
                .as_deref()
                .is_some_and(|path| self.path_matches(path))
            || self.cgroup_matches(&record.cgroup)
    }

    fn possible_match(&self, record: &ProcessRecord) -> bool {
        let command = record.command_name.to_lowercase();
        let argv_name = record
            .argv0
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        (!self.executable_name.is_empty()
            && (command == self.executable_name || argv_name == self.executable_name))
            || (!self.startup_class.is_empty()
                && (command == self.startup_class || argv_name == self.startup_class))
    }

    fn path_matches(&self, candidate: &Path) -> bool {
        if self.executable_token.as_deref() == Some(candidate) {
            return true;
        }
        let Some(resolved) = self.resolved_executable.as_deref() else {
            return false;
        };
        canonical_or_original(candidate).as_deref() == Some(resolved)
    }

    fn cgroup_matches(&self, cgroup: &str) -> bool {
        if !cgroup.contains("app") || !cgroup.contains(".scope") {
            return false;
        }
        let decoded = decode_systemd_escapes(cgroup).to_lowercase();
        self.desktop_keys
            .iter()
            .any(|key| contains_identifier(&decoded, key))
    }
}

pub fn identify(pid: u32) -> Option<ProcessIdentity> {
    identify_at(Path::new("/proc"), geteuid().as_raw(), pid)
}

fn identify_at(proc_root: &Path, current_uid: u32, pid: u32) -> Option<ProcessIdentity> {
    let record = read_process(proc_root, pid).ok()?;
    (record.uid == current_uid).then_some(record.identity)
}

pub fn scan_application(
    application: &InstalledApplication,
    known_launches: &[ProcessIdentity],
) -> io::Result<ApplicationProcessState> {
    scan_application_at(
        Path::new("/proc"),
        geteuid().as_raw(),
        application,
        known_launches,
    )
}

pub fn inspect_referenced_pid(
    application: &InstalledApplication,
    known_launches: &[ProcessIdentity],
    pid: u32,
) -> ReferencedProcess {
    let proc_root = Path::new("/proc");
    let record = match read_process(proc_root, pid) {
        Ok(record) if record.uid == geteuid().as_raw() => record,
        Ok(record) => return ReferencedProcess::Unrelated(record.identity),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ReferencedProcess::Missing,
        Err(_) if !proc_root.join(pid.to_string()).exists() => return ReferencedProcess::Missing,
        Err(_) => return ReferencedProcess::Unreadable,
    };
    match scan_application(application, known_launches) {
        Ok(state) if state.contains_verified(record.identity) => {
            ReferencedProcess::Verified(record.identity)
        }
        Ok(state)
            if state
                .possible
                .iter()
                .any(|possible| possible.identity == record.identity) =>
        {
            ReferencedProcess::Possible(record.identity)
        }
        Ok(_) => ReferencedProcess::Unrelated(record.identity),
        Err(_) => ReferencedProcess::Unreadable,
    }
}

pub fn terminate_session(
    application: &InstalledApplication,
    known_launches: &[ProcessIdentity],
    session: &ApplicationSession,
    mode: TerminationMode,
) -> io::Result<TerminationReport> {
    terminate_session_with(
        Path::new("/proc"),
        geteuid().as_raw(),
        application,
        known_launches,
        session,
        mode,
        &mut RustixSignalSender,
    )
}

trait SignalSender {
    fn send(
        &mut self,
        proc_root: &Path,
        current_uid: u32,
        identity: ProcessIdentity,
        mode: TerminationMode,
    ) -> io::Result<()>;
}

struct RustixSignalSender;

impl SignalSender for RustixSignalSender {
    fn send(
        &mut self,
        proc_root: &Path,
        current_uid: u32,
        identity: ProcessIdentity,
        mode: TerminationMode,
    ) -> io::Result<()> {
        let pid = i32::try_from(identity.pid)
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process ID"))?;
        let pidfd = pidfd_open(pid, PidfdFlags::empty())?;
        if identify_at(proc_root, current_uid, identity.pid) != Some(identity) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "process identity changed",
            ));
        }
        let signal = match mode {
            TerminationMode::Graceful => Signal::TERM,
            TerminationMode::Force => Signal::KILL,
        };
        pidfd_send_signal(pidfd, signal)?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn terminate_session_with(
    proc_root: &Path,
    current_uid: u32,
    application: &InstalledApplication,
    known_launches: &[ProcessIdentity],
    session: &ApplicationSession,
    mode: TerminationMode,
    sender: &mut impl SignalSender,
) -> io::Result<TerminationReport> {
    let mut trusted = known_launches.to_vec();
    trusted.extend(session.identities());
    trusted.sort_unstable_by_key(|identity| (identity.pid, identity.start_time_ticks));
    trusted.dedup();

    let state = scan_application_at(proc_root, current_uid, application, &trusted)?;
    let original: HashSet<_> = session.identities().into_iter().collect();
    let current = state.sessions.into_iter().find(|candidate| {
        candidate
            .identities()
            .iter()
            .any(|identity| original.contains(identity))
    });
    let Some(current) = current else {
        return Ok(TerminationReport::default());
    };

    let mut report = TerminationReport::default();
    for identity in current.identities().into_iter().rev() {
        if identify_at(proc_root, current_uid, identity.pid) != Some(identity) {
            report.failed += 1;
            continue;
        }
        match sender.send(proc_root, current_uid, identity, mode) {
            Ok(()) => report.signaled += 1,
            Err(_) => report.failed += 1,
        }
    }
    Ok(report)
}

fn scan_application_at(
    proc_root: &Path,
    current_uid: u32,
    application: &InstalledApplication,
    known_launches: &[ProcessIdentity],
) -> io::Result<ApplicationProcessState> {
    let records = read_process_table(proc_root)?;
    Ok(classify_processes(
        records,
        current_uid,
        &ApplicationIdentity::from_application(application),
        known_launches,
    ))
}

fn classify_processes(
    records: Vec<ProcessRecord>,
    current_uid: u32,
    application: &ApplicationIdentity,
    known_launches: &[ProcessIdentity],
) -> ApplicationProcessState {
    let records: HashMap<u32, ProcessRecord> = records
        .into_iter()
        .filter(|record| record.uid == current_uid)
        .map(|record| (record.identity.pid, record))
        .collect();
    let known: HashSet<_> = known_launches.iter().copied().collect();
    let verified_seeds: HashSet<u32> = records
        .values()
        .filter(|record| known.contains(&record.identity) || application.verified_match(record))
        .map(|record| record.identity.pid)
        .collect();

    let root_seeds: Vec<u32> = verified_seeds
        .iter()
        .copied()
        .filter(|pid| !has_verified_ancestor(*pid, &records, &verified_seeds))
        .collect();
    let mut claimed = HashSet::new();
    let mut sessions = Vec::new();
    for root_pid in root_seeds {
        let members = descendants(root_pid, &records);
        claimed.extend(members.iter().copied());
        if let Some(root) = build_tree(root_pid, &records, &members, true) {
            sessions.push(ApplicationSession { root });
        }
    }
    sessions.sort_by_key(|session| session.root.identity.pid);

    let mut possible = records
        .values()
        .filter(|record| {
            !claimed.contains(&record.identity.pid)
                && !verified_seeds.contains(&record.identity.pid)
                && application.possible_match(record)
        })
        .map(|record| PossibleProcess {
            identity: record.identity,
            executable: record.display_executable(),
        })
        .collect::<Vec<_>>();
    possible.sort_by_key(|process| process.identity.pid);

    ApplicationProcessState { sessions, possible }
}

fn has_verified_ancestor(
    pid: u32,
    records: &HashMap<u32, ProcessRecord>,
    verified: &HashSet<u32>,
) -> bool {
    let mut current = records.get(&pid).map(|record| record.parent_pid);
    let mut visited = HashSet::new();
    while let Some(parent) = current.filter(|parent| *parent != 0) {
        if !visited.insert(parent) {
            break;
        }
        if verified.contains(&parent) {
            return true;
        }
        current = records.get(&parent).map(|record| record.parent_pid);
    }
    false
}

fn descendants(root_pid: u32, records: &HashMap<u32, ProcessRecord>) -> HashSet<u32> {
    let mut members = HashSet::from([root_pid]);
    loop {
        let before = members.len();
        for record in records.values() {
            if members.contains(&record.parent_pid) {
                members.insert(record.identity.pid);
            }
        }
        if members.len() == before {
            break;
        }
    }
    members
}

fn build_tree(
    pid: u32,
    records: &HashMap<u32, ProcessRecord>,
    members: &HashSet<u32>,
    main: bool,
) -> Option<ProcessNode> {
    let record = records.get(&pid)?;
    let mut child_pids = records
        .values()
        .filter(|candidate| {
            candidate.parent_pid == pid && members.contains(&candidate.identity.pid)
        })
        .map(|candidate| candidate.identity.pid)
        .collect::<Vec<_>>();
    child_pids.sort_unstable();
    let children = child_pids
        .into_iter()
        .filter_map(|child| build_tree(child, records, members, false))
        .collect();
    Some(ProcessNode {
        identity: record.identity,
        executable: record.display_executable(),
        role: if main {
            ProcessRole::Main
        } else {
            ProcessRole::Child
        },
        children,
    })
}

fn read_process_table(proc_root: &Path) -> io::Result<Vec<ProcessRecord>> {
    let mut records = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(record) = read_process(proc_root, pid) {
            records.push(record);
        }
    }
    Ok(records)
}

fn read_process(proc_root: &Path, pid: u32) -> io::Result<ProcessRecord> {
    let directory = proc_root.join(pid.to_string());
    let stat = fs::read_to_string(directory.join("stat"))?;
    let (parent_pid, start_time_ticks, command_name) = parse_stat(&stat)?;
    let status = fs::read_to_string(directory.join("status"))?;
    let uid = parse_effective_uid(&status)?;
    let executable_path = fs::read_link(directory.join("exe")).ok();
    let argv0 = fs::read(directory.join("cmdline"))
        .ok()
        .and_then(|bytes| bytes.split(|byte| *byte == 0).next().map(Vec::from))
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| PathBuf::from(std::ffi::OsString::from_vec(bytes)));
    let cgroup = fs::read_to_string(directory.join("cgroup")).unwrap_or_default();
    Ok(ProcessRecord {
        identity: ProcessIdentity {
            pid,
            start_time_ticks,
        },
        parent_pid,
        uid,
        executable_path,
        argv0,
        command_name,
        cgroup,
    })
}

fn parse_stat(stat: &str) -> io::Result<(u32, u64, String)> {
    let open = stat.find('(').ok_or_else(invalid_proc_data)?;
    let close = stat.rfind(')').ok_or_else(invalid_proc_data)?;
    if close <= open {
        return Err(invalid_proc_data());
    }
    let command = stat[open + 1..close].to_owned();
    let fields = stat[close + 1..].split_whitespace().collect::<Vec<_>>();
    let parent_pid = fields
        .get(1)
        .and_then(|value| value.parse().ok())
        .ok_or_else(invalid_proc_data)?;
    let start_time_ticks = fields
        .get(19)
        .and_then(|value| value.parse().ok())
        .ok_or_else(invalid_proc_data)?;
    Ok((parent_pid, start_time_ticks, command))
}

fn parse_effective_uid(status: &str) -> io::Result<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|values| values.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .ok_or_else(invalid_proc_data)
}

fn invalid_proc_data() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid process metadata")
}

fn resolve_executable(path: &Path) -> Option<PathBuf> {
    if path.components().count() > 1 {
        return Some(path.to_owned());
    }
    env::var_os("PATH").and_then(|path_env| {
        env::split_paths(&path_env)
            .map(|directory| directory.join(path))
            .find(|candidate| candidate.is_file())
    })
}

fn canonical_or_original(path: &Path) -> Option<PathBuf> {
    path.canonicalize()
        .ok()
        .or_else(|| path.exists().then(|| path.to_owned()))
}

fn decode_systemd_escapes(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && bytes.get(index + 1) == Some(&b'x')
            && let Some(hex) = bytes.get(index + 2..index + 4)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(decoded) = u8::from_str_radix(hex, 16)
        {
            output.push(decoded);
            index += 4;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn contains_identifier(value: &str, identifier: &str) -> bool {
    value.match_indices(identifier).any(|(start, matched)| {
        let before = value[..start].chars().next_back();
        let after = value[start + matched.len()..].chars().next();
        before.is_none_or(|character| !is_identifier_character(character))
            && after.is_none_or(|character| !is_identifier_character(character))
    })
}

fn is_identifier_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn record(pid: u32, parent_pid: u32, executable: &str) -> ProcessRecord {
        ProcessRecord {
            identity: ProcessIdentity {
                pid,
                start_time_ticks: u64::from(pid) * 10,
            },
            parent_pid,
            uid: 1000,
            executable_path: Some(PathBuf::from(executable)),
            argv0: Some(PathBuf::from(executable)),
            command_name: Path::new(executable)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            cgroup: String::new(),
        }
    }

    fn application() -> ApplicationIdentity {
        ApplicationIdentity {
            executable_token: Some(PathBuf::from("/usr/bin/example")),
            resolved_executable: Some(PathBuf::from("/usr/bin/example")),
            executable_name: "example".into(),
            startup_class: "example".into(),
            desktop_keys: vec!["org.example.app".into()],
        }
    }

    fn installed_application() -> InstalledApplication {
        InstalledApplication {
            desktop_id: "org.example.App.desktop".into(),
            display_name: "Example".into(),
            summary: None,
            description: None,
            icon_name: "example".into(),
            desktop_file: None,
            executable: Some("/usr/bin/example".into()),
            startup_wm_class: Some("example".into()),
            owner: None,
            related_desktop_ids: vec![],
        }
    }

    struct ProcFixture(PathBuf);

    impl ProcFixture {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "duck-packages-proc-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn process(
            &self,
            pid: u32,
            parent_pid: u32,
            start_time_ticks: u64,
            uid: u32,
            executable: &str,
        ) {
            let directory = self.0.join(pid.to_string());
            fs::create_dir_all(&directory).unwrap();
            let mut fields = vec!["S".to_owned(), parent_pid.to_string()];
            fields.extend(std::iter::repeat_n("0".to_owned(), 17));
            fields.push(start_time_ticks.to_string());
            fs::write(
                directory.join("stat"),
                format!("{pid} (example) {}", fields.join(" ")),
            )
            .unwrap();
            fs::write(
                directory.join("status"),
                format!("Name:\texample\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
            )
            .unwrap();
            let mut cmdline = executable.as_bytes().to_vec();
            cmdline.push(0);
            fs::write(directory.join("cmdline"), cmdline).unwrap();
            fs::write(directory.join("cgroup"), "0::/user.slice/app.slice\n").unwrap();
            let exe = directory.join("exe");
            if exe.symlink_metadata().is_ok() {
                fs::remove_file(&exe).unwrap();
            }
            symlink(executable, exe).unwrap();
        }
    }

    impl Drop for ProcFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct RecordingSignalSender(Vec<(u32, TerminationMode)>);

    impl SignalSender for RecordingSignalSender {
        fn send(
            &mut self,
            _proc_root: &Path,
            _current_uid: u32,
            identity: ProcessIdentity,
            mode: TerminationMode,
        ) -> io::Result<()> {
            self.0.push((identity.pid, mode));
            Ok(())
        }
    }

    #[test]
    fn parses_stat_with_spaces_and_parentheses_in_command() {
        let mut fields = vec!["S", "7"];
        fields.extend(std::iter::repeat_n("0", 17));
        fields.push("9876");
        let stat = format!("42 (name with ) parenthesis) {}", fields.join(" "));
        assert_eq!(
            parse_stat(&stat).unwrap(),
            (7, 9876, "name with ) parenthesis".into())
        );
    }

    #[test]
    fn verified_root_collects_same_user_descendants() {
        let state = classify_processes(
            vec![
                record(10, 1, "/usr/bin/example"),
                record(11, 10, "/usr/lib/example/helper"),
                record(12, 11, "/usr/lib/example/renderer"),
            ],
            1000,
            &application(),
            &[],
        );
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].process_count(), 3);
        assert!(state.possible.is_empty());
    }

    #[test]
    fn independent_roots_become_independent_sessions() {
        let state = classify_processes(
            vec![
                record(10, 1, "/usr/bin/example"),
                record(20, 1, "/usr/bin/example"),
            ],
            1000,
            &application(),
            &[],
        );
        assert_eq!(state.sessions.len(), 2);
    }

    #[test]
    fn weak_name_match_is_read_only() {
        let mut process = record(10, 1, "/opt/other/example");
        process.executable_path = None;
        process.argv0 = None;
        let state = classify_processes(vec![process], 1000, &application(), &[]);
        assert!(state.sessions.is_empty());
        assert_eq!(state.possible.len(), 1);
    }

    #[test]
    fn launch_identity_must_include_start_time() {
        let process = record(10, 1, "/opt/unrelated");
        let reused = ProcessIdentity {
            pid: 10,
            start_time_ticks: 5,
        };
        let state = classify_processes(vec![process], 1000, &application(), &[reused]);
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn cgroup_desktop_id_is_verified_after_systemd_unescape() {
        let mut process = record(10, 1, "/opt/app/bin");
        process.cgroup = "0::/user.slice/app.slice/app-gnome-org.example.app\\x2d123.scope".into();
        let state = classify_processes(vec![process], 1000, &application(), &[]);
        assert_eq!(state.sessions.len(), 1);
    }

    #[test]
    fn cgroup_desktop_id_requires_identifier_boundaries() {
        let mut process = record(10, 1, "/opt/app/bin");
        process.cgroup =
            "0::/user.slice/app.slice/app-gnome-org.example.application-123.scope".into();
        let state = classify_processes(vec![process], 1000, &application(), &[]);
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn another_user_is_never_included() {
        let mut process = record(10, 1, "/usr/bin/example");
        process.uid = 1001;
        let state = classify_processes(vec![process], 1000, &application(), &[]);
        assert!(state.is_empty());
    }

    #[test]
    fn proc_fixture_is_parsed_and_signals_are_injectable() {
        let fixture = ProcFixture::new();
        fixture.process(10, 1, 100, 1000, "/usr/bin/example");
        fixture.process(11, 10, 110, 1000, "/usr/lib/example/helper");
        let app = installed_application();
        let state = scan_application_at(&fixture.0, 1000, &app, &[]).unwrap();
        assert_eq!(state.sessions[0].process_count(), 2);

        let mut sender = RecordingSignalSender::default();
        let report = terminate_session_with(
            &fixture.0,
            1000,
            &app,
            &[],
            &state.sessions[0],
            TerminationMode::Graceful,
            &mut sender,
        )
        .unwrap();
        assert_eq!(report.signaled, 2);
        assert_eq!(
            sender.0,
            vec![
                (11, TerminationMode::Graceful),
                (10, TerminationMode::Graceful)
            ]
        );
        assert!(
            sender
                .0
                .iter()
                .all(|(_, mode)| *mode == TerminationMode::Graceful)
        );
    }

    #[test]
    fn reused_pid_is_revalidated_before_signaling() {
        let fixture = ProcFixture::new();
        fixture.process(10, 1, 100, 1000, "/usr/bin/example");
        let app = installed_application();
        let state = scan_application_at(&fixture.0, 1000, &app, &[]).unwrap();
        fixture.process(10, 1, 999, 1000, "/usr/bin/example");

        let mut sender = RecordingSignalSender::default();
        let report = terminate_session_with(
            &fixture.0,
            1000,
            &app,
            &[],
            &state.sessions[0],
            TerminationMode::Force,
            &mut sender,
        )
        .unwrap();
        assert_eq!(report, TerminationReport::default());
        assert!(sender.0.is_empty());
    }
}
