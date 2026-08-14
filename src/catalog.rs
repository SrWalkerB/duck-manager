use std::{
    collections::HashMap,
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::process::ExitStatusExt,
    },
    path::Path,
    process::ExitStatus,
};

use gio::prelude::*;
use glib::prelude::Cast;
use libappstream::{
    LaunchableKind, Pool,
    prelude::{ComponentBoxExt, ComponentExt, PoolExt},
};

use crate::domain::InstalledApplication;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticStream {
    Stdout,
    Stderr,
}

impl DiagnosticStream {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiagnosticEvent {
    ProcessStarted(u32),
    ProcessIdUnavailable,
    Output {
        stream: DiagnosticStream,
        bytes: Vec<u8>,
    },
    StreamFailed {
        stream: DiagnosticStream,
        message: String,
    },
    ProcessExited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

pub struct DiagnosticSession {
    pub events: async_channel::Receiver<DiagnosticEvent>,
}

pub async fn load_applications() -> Vec<InstalledApplication> {
    let pool = Pool::new();
    let worker_pool = pool.clone();
    let _ = blocking::unblock(move || worker_pool.load(None::<&gio::Cancellable>)).await;

    let applications = gio::AppInfo::all()
        .into_iter()
        .filter_map(|info| info.downcast::<gio::DesktopAppInfo>().ok())
        .filter(|info| info.should_show() && !info.is_nodisplay())
        .filter(|info| {
            info.filename()
                .as_deref()
                .is_some_and(is_distribution_desktop_file)
        })
        .map(|info| from_desktop_info(&info, &pool))
        .collect();

    deduplicate(applications)
}

fn from_desktop_info(info: &gio::DesktopAppInfo, pool: &Pool) -> InstalledApplication {
    let desktop_id = info
        .id()
        .map(|id| id.to_string())
        .or_else(|| {
            info.filename().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| "unknown.desktop".into());
    let metadata = pool
        .components_by_launchable(LaunchableKind::DesktopId, &desktop_id)
        .and_then(|components| components.index_safe(0));

    let desktop_description = info.description().map(|value| value.to_string());
    let summary = metadata
        .as_ref()
        .and_then(ComponentExt::summary)
        .map(|value| value.to_string())
        .or_else(|| desktop_description.clone());
    let description = metadata
        .as_ref()
        .and_then(ComponentExt::description)
        .map(|value| strip_markup(&value))
        .or(desktop_description);

    InstalledApplication {
        desktop_id,
        display_name: info.display_name().to_string(),
        summary,
        description,
        icon_name: info
            .icon()
            .and_then(|icon| icon.to_string())
            .map(|value| value.to_string())
            .unwrap_or_else(|| "package-x-generic-symbolic".into()),
        desktop_file: info.filename(),
        executable: Some(info.executable().to_string_lossy().into_owned()),
        startup_wm_class: info.startup_wm_class().map(|value| value.to_string()),
        owner: None,
        related_desktop_ids: Vec::new(),
    }
}

fn is_distribution_desktop_file(path: &Path) -> bool {
    let path = path.to_string_lossy();
    (path.starts_with("/usr/share/applications/")
        || path.starts_with("/usr/local/share/applications/"))
        && !path.contains("/flatpak/")
        && !path.contains("/snap/")
}

pub fn deduplicate(applications: Vec<InstalledApplication>) -> Vec<InstalledApplication> {
    let mut unique: HashMap<String, InstalledApplication> = HashMap::new();
    for application in applications {
        let key = deduplication_key(&application);
        match unique.get_mut(&key) {
            Some(existing) => {
                existing
                    .related_desktop_ids
                    .push(application.desktop_id.clone());
                if existing.summary.is_none() {
                    existing.summary = application.summary;
                }
                if existing.description.is_none() {
                    existing.description = application.description;
                }
            }
            None => {
                unique.insert(key, application);
            }
        }
    }
    let mut applications: Vec<_> = unique.into_values().collect();
    applications.sort_by_key(|application| application.display_name.to_lowercase());
    applications
}

fn deduplication_key(application: &InstalledApplication) -> String {
    let executable = application
        .executable
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let startup_class = application
        .startup_wm_class
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();
    let name = normalize_name(&application.display_name);
    format!("{executable}|{startup_class}|{name}")
}

fn normalize_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn strip_markup(value: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(character),
            _ => {}
        }
    }
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn launch(application: &InstalledApplication) -> Result<Option<u32>, glib::Error> {
    let info = resolve_launcher(application).ok_or_else(launcher_not_found)?;
    let mut pid = None;
    info.launch_uris_as_manager(
        &[],
        None::<&gio::AppLaunchContext>,
        glib::SpawnFlags::DO_NOT_REAP_CHILD,
        None,
        Some(&mut |_, child_pid| pid = Some(child_pid)),
    )?;
    if let Some(child_pid) = pid {
        glib::source::child_watch_add_local(child_pid, |_, _| {});
    }
    Ok(pid.map(|pid| pid.0 as u32))
}

pub fn launch_with_logs(
    application: &InstalledApplication,
) -> Result<DiagnosticSession, glib::Error> {
    let info = resolve_launcher(application).ok_or_else(launcher_not_found)?;
    let (stdout_read, stdout_write) = open_pipe()?;
    let (stderr_read, stderr_write) = open_pipe()?;
    let (sender, events) = async_channel::unbounded();
    let mut pid = None;

    info.launch_uris_as_manager_with_fds(
        &[],
        None::<&gio::AppLaunchContext>,
        glib::SpawnFlags::DO_NOT_REAP_CHILD,
        None,
        Some(&mut |_, child_pid| pid = Some(child_pid)),
        None::<&OwnedFd>,
        Some(&stdout_write),
        Some(&stderr_write),
    )?;
    drop(stdout_write);
    drop(stderr_write);

    spawn_stream_reader(stdout_read, DiagnosticStream::Stdout, sender.clone());
    spawn_stream_reader(stderr_read, DiagnosticStream::Stderr, sender.clone());

    if let Some(pid) = pid {
        let _ = sender.try_send(DiagnosticEvent::ProcessStarted(pid.0 as u32));
        glib::source::child_watch_add_local(pid, move |_, wait_status| {
            let status = ExitStatus::from_raw(wait_status);
            let _ = sender.try_send(DiagnosticEvent::ProcessExited {
                code: status.code(),
                signal: status.signal(),
            });
        });
    } else {
        let _ = sender.try_send(DiagnosticEvent::ProcessIdUnavailable);
    }

    Ok(DiagnosticSession { events })
}

fn resolve_launcher(application: &InstalledApplication) -> Option<gio::DesktopAppInfo> {
    application
        .desktop_file
        .as_ref()
        .and_then(gio::DesktopAppInfo::from_filename)
        .or_else(|| gio::DesktopAppInfo::new(&application.desktop_id))
}

fn launcher_not_found() -> glib::Error {
    glib::Error::new(
        gio::IOErrorEnum::NotFound,
        "the desktop launcher is no longer available",
    )
}

fn open_pipe() -> Result<(OwnedFd, OwnedFd), glib::Error> {
    let (read, write) = glib::unix_open_pipe(0)?;
    // SAFETY: g_unix_open_pipe returns two newly owned file descriptors.
    Ok(unsafe { (OwnedFd::from_raw_fd(read), OwnedFd::from_raw_fd(write)) })
}

fn spawn_stream_reader(
    fd: OwnedFd,
    stream: DiagnosticStream,
    sender: async_channel::Sender<DiagnosticEvent>,
) {
    let input = gio::UnixInputStream::take_fd(fd);
    glib::MainContext::default().spawn_local(async move {
        loop {
            match input
                .read_bytes_future(8 * 1024, glib::Priority::DEFAULT)
                .await
            {
                Ok(bytes) if bytes.is_empty() => break,
                Ok(bytes) => {
                    if sender
                        .send(DiagnosticEvent::Output {
                            stream,
                            bytes: bytes.as_ref().to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender
                        .send(DiagnosticEvent::StreamFailed {
                            stream,
                            message: error.to_string(),
                        })
                        .await;
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chrome(id: &str) -> InstalledApplication {
        InstalledApplication {
            desktop_id: id.into(),
            display_name: "Google Chrome".into(),
            summary: None,
            description: None,
            icon_name: "google-chrome".into(),
            desktop_file: None,
            executable: Some("/usr/bin/google-chrome-stable".into()),
            startup_wm_class: Some("google-chrome".into()),
            owner: None,
            related_desktop_ids: vec![],
        }
    }

    #[test]
    fn chrome_aliases_become_one_application() {
        let result = deduplicate(vec![
            chrome("google-chrome.desktop"),
            chrome("com.google.Chrome.desktop"),
        ]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].related_desktop_ids.len(), 1);
    }

    #[test]
    fn flatpak_and_user_launchers_are_not_distribution_packages() {
        assert!(!is_distribution_desktop_file(Path::new(
            "/var/lib/flatpak/exports/share/applications/org.example.App.desktop"
        )));
        assert!(!is_distribution_desktop_file(Path::new(
            "/home/user/.local/share/applications/AppImage.desktop"
        )));
        assert!(is_distribution_desktop_file(Path::new(
            "/usr/share/applications/google-chrome.desktop"
        )));
    }

    #[test]
    fn appstream_markup_becomes_plain_readable_text() {
        assert_eq!(
            strip_markup("<p>Play <em>audio</em> files.</p>"),
            "Play audio files."
        );
    }

    #[test]
    fn diagnostic_streams_have_stable_labels() {
        assert_eq!(DiagnosticStream::Stdout.label(), "stdout");
        assert_eq!(DiagnosticStream::Stderr.label(), "stderr");
    }

    #[test]
    fn diagnostic_launch_reports_a_missing_launcher() {
        let application = InstalledApplication {
            desktop_id: "io.github.DuckPackages.DoesNotExist.desktop".into(),
            display_name: "Missing".into(),
            summary: None,
            description: None,
            icon_name: "application-x-executable-symbolic".into(),
            desktop_file: None,
            executable: None,
            startup_wm_class: None,
            owner: None,
            related_desktop_ids: vec![],
        };
        assert!(launch_with_logs(&application).is_err());
    }
}
