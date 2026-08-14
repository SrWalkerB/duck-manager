#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchProblem {
    ProfileLocked { referenced_pid: Option<u32> },
    PermissionDenied,
    RequiredFileMissing,
    GraphicalSessionUnavailable,
}

#[derive(Default)]
pub struct LaunchProblemAnalyzer {
    partial: String,
}

impl LaunchProblemAnalyzer {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<LaunchProblem> {
        self.partial.push_str(&String::from_utf8_lossy(bytes));
        let mut problems = Vec::new();
        while let Some(newline) = self.partial.find('\n') {
            let line = self.partial[..newline].to_owned();
            self.partial.drain(..=newline);
            if let Some(problem) = analyze_text(&line) {
                problems.push(problem);
            }
        }
        if self.partial.len() > 16 * 1024 {
            if let Some(problem) = analyze_text(&self.partial) {
                problems.push(problem);
            }
            self.partial.clear();
        }
        problems
    }

    pub fn finish(&mut self) -> Vec<LaunchProblem> {
        let problem = analyze_text(&self.partial).into_iter().collect();
        self.partial.clear();
        problem
    }
}

pub fn analyze_text(text: &str) -> Option<LaunchProblem> {
    let lower = text.to_lowercase();

    if lower.contains("profile")
        && (lower.contains("in use by another")
            || lower.contains("profile is locked")
            || lower.contains("locked the profile"))
    {
        return Some(LaunchProblem::ProfileLocked {
            referenced_pid: extract_pid_references(text).into_iter().next(),
        });
    }
    if lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("access denied")
    {
        return Some(LaunchProblem::PermissionDenied);
    }
    if lower.contains("error while loading shared libraries")
        || lower.contains("cannot open shared object file")
        || lower.contains("no such file or directory")
        || lower.contains("file not found")
    {
        return Some(LaunchProblem::RequiredFileMissing);
    }
    if lower.contains("cannot open display")
        || lower.contains("failed to open display")
        || lower.contains("unable to open display")
        || lower.contains("no display server")
    {
        return Some(LaunchProblem::GraphicalSessionUnavailable);
    }
    None
}

pub fn extract_pid_references(line: &str) -> Vec<u32> {
    let lower = line.to_lowercase();
    let mut pids = Vec::new();
    for marker in ["pid", "process"] {
        let mut offset = 0;
        while let Some(found) = lower[offset..].find(marker) {
            let start = offset + found;
            let before_is_word = start
                .checked_sub(1)
                .and_then(|index| lower.as_bytes().get(index))
                .is_some_and(u8::is_ascii_alphanumeric);
            let after = start + marker.len();
            let after_is_word = lower
                .as_bytes()
                .get(after)
                .is_some_and(u8::is_ascii_alphanumeric);
            if !before_is_word && !after_is_word {
                let tail = lower[after..].trim_start();
                let tail = tail
                    .strip_prefix('=')
                    .or_else(|| tail.strip_prefix(':'))
                    .unwrap_or(tail)
                    .trim_start();
                let tail = tail.strip_prefix('(').unwrap_or(tail);
                let digits = tail
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>();
                if let Ok(pid) = digits.parse::<u32>()
                    && pid > 0
                {
                    pids.push(pid);
                }
            }
            offset = after;
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_a_generic_profile_lock_and_its_pid() {
        let line = "The profile appears to be in use by another browser process (18956). The application has locked the profile to prevent corruption.";
        assert_eq!(
            analyze_text(line),
            Some(LaunchProblem::ProfileLocked {
                referenced_pid: Some(18956)
            })
        );
    }

    #[test]
    fn analyzer_handles_a_problem_split_between_chunks() {
        let mut analyzer = LaunchProblemAnalyzer::default();
        assert!(
            analyzer
                .push(b"The profile appears to be in use by another pro")
                .is_empty()
        );
        assert_eq!(
            analyzer.push(b"cess (42). The application locked the profile.\n"),
            vec![LaunchProblem::ProfileLocked {
                referenced_pid: Some(42)
            }]
        );
    }

    #[test]
    fn recognizes_common_failure_categories_without_application_rules() {
        assert_eq!(
            analyze_text("failed to start: Permission denied"),
            Some(LaunchProblem::PermissionDenied)
        );
        assert_eq!(
            analyze_text(
                "error while loading shared libraries: libdemo.so: cannot open shared object file"
            ),
            Some(LaunchProblem::RequiredFileMissing)
        );
        assert_eq!(
            analyze_text("Gtk-WARNING: cannot open display: :0"),
            Some(LaunchProblem::GraphicalSessionUnavailable)
        );
    }

    #[test]
    fn unrelated_errors_are_not_misdiagnosed() {
        assert_eq!(analyze_text("Process exited with code 21"), None);
    }
}
