use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::Arc,
};

use adw::prelude::*;
use futures_util::StreamExt;
use gtk::{Align, Orientation, PolicyType, SelectionMode};

use crate::{
    backend::{BackendFactory, PackageBackend},
    catalog,
    domain::{
        BackendCapabilities, BackendError, BackendKind, InstalledApplication, InstalledPackage,
        RemovalPlan, RemovalRequest, RestartRequirement, TransactionEvent, format_size,
    },
    launch_diagnostic::{self, LaunchProblem, LaunchProblemAnalyzer},
    processes::{
        self, ApplicationProcessState, ApplicationSession, ProcessIdentity, ProcessNode,
        ProcessRole, ReferencedProcess, TerminationMode,
    },
    profile_locks::{self, CleanupError, StaleProfileLock},
    tr, trn, version,
};

const DIAGNOSTIC_LOG_LIMIT: usize = 1024 * 1024;

#[derive(Clone)]
struct AppState {
    window: adw::ApplicationWindow,
    navigation: adw::NavigationView,
    toast_overlay: adw::ToastOverlay,
    search: gtk::SearchEntry,
    apps_list: CatalogList,
    apps_stack: gtk::Stack,
    profile_lock_panel: ProfileLockPanel,
    packages_list: CatalogList,
    packages_stack: gtk::Stack,
    view_stack: adw::ViewStack,
    search_generation: Rc<Cell<u64>>,
    backend: Arc<dyn PackageBackend>,
    capabilities: BackendCapabilities,
    applications: Rc<RefCell<Vec<InstalledApplication>>>,
    packages: Rc<RefCell<Vec<InstalledPackage>>>,
    tracked_processes: Rc<RefCell<HashMap<String, Vec<ProcessIdentity>>>>,
}

#[derive(Clone)]
struct CatalogList {
    scroller: gtk::ScrolledWindow,
    list: gtk::ListBox,
}

#[derive(Clone)]
struct ProfileLockPanel {
    root: gtk::Box,
    list: gtk::ListBox,
}

impl ProfileLockPanel {
    fn new() -> Self {
        let root = gtk::Box::new(Orientation::Vertical, 8);
        root.set_margin_top(12);
        root.set_margin_start(24);
        root.set_margin_end(24);
        root.set_visible(false);

        let header = gtk::Box::new(Orientation::Vertical, 2);
        let title = gtk::Label::builder()
            .label(tr("Startup problems"))
            .halign(Align::Start)
            .xalign(0.0)
            .build();
        title.add_css_class("heading");
        let subtitle = gtk::Label::builder()
            .label(tr("These applications have stale profile locks."))
            .halign(Align::Start)
            .xalign(0.0)
            .wrap(true)
            .build();
        subtitle.add_css_class("dim-label");
        header.append(&title);
        header.append(&subtitle);

        let list = gtk::ListBox::builder()
            .selection_mode(SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        root.append(&header);
        root.append(&list);
        Self { root, list }
    }

    fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn refresh(&self, state: &AppState) {
        let panel = self.clone();
        let state = state.clone();
        glib::MainContext::default().spawn_local(async move {
            match blocking::unblock(profile_locks::scan).await {
                Ok(locks) => panel.render(&state, locks),
                Err(error) => panel.show_error(&error.to_string()),
            }
        });
    }

    fn render(&self, state: &AppState, locks: Vec<StaleProfileLock>) {
        clear_list_box(&self.list);
        self.root.set_visible(!locks.is_empty());
        for lock in locks {
            let app_name = profile_lock_application_name(state, &lock);
            let subtitle = tr("Profile {profile} · PID {pid} · {count} stale lock(s)")
                .replace("{profile}", &lock.profile_name())
                .replace("{pid}", &lock.pid.to_string())
                .replace("{count}", &lock.markers.len().to_string());
            let row = adw::ActionRow::builder()
                .title(&app_name)
                .subtitle(subtitle)
                .title_lines(1)
                .subtitle_lines(1)
                .selectable(false)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
            let clean = gtk::Button::with_label(&tr("Clean up"));
            clean.set_valign(Align::Center);
            clean.add_css_class("suggested-action");
            let panel = self.clone();
            let state_for_action = state.clone();
            clean.connect_clicked(move |_| {
                panel.confirm_cleanup(&state_for_action, lock.clone(), app_name.clone());
            });
            row.add_suffix(&clean);
            self.list.append(&row);
        }
    }

    fn show_error(&self, message: &str) {
        clear_list_box(&self.list);
        self.root.set_visible(true);
        let row = adw::ActionRow::builder()
            .title(tr("Could not inspect profile locks"))
            .subtitle(message)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        self.list.append(&row);
    }

    fn confirm_cleanup(&self, state: &AppState, lock: StaleProfileLock, app_name: String) {
        let body = tr("Remove only the stale SingletonLock, SingletonCookie and SingletonSocket symlinks for {application}? Profile data such as bookmarks, history and settings will not be changed.")
            .replace("{application}", &app_name);
        let dialog = adw::AlertDialog::builder()
            .heading(tr("Clean up stale profile lock?"))
            .body(body)
            .close_response("cancel")
            .default_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("clean", &tr("Clean up"))]);
        dialog.set_response_appearance("clean", adw::ResponseAppearance::Suggested);
        let panel = self.clone();
        let state = state.clone();
        let parent = state.window.clone();
        dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |response| {
            if response == "clean" {
                panel.execute_cleanup(&state, lock.clone());
            }
        });
    }

    fn execute_cleanup(&self, state: &AppState, lock: StaleProfileLock) {
        let panel = self.clone();
        let state = state.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = blocking::unblock(move || profile_locks::cleanup(&lock)).await;
            match result {
                Ok(count) => {
                    state.toast_overlay.add_toast(adw::Toast::new(
                        &tr("Removed {count} stale profile lock(s).")
                            .replace("{count}", &count.to_string()),
                    ));
                    panel.refresh(&state);
                }
                Err(error) => {
                    let message = match error {
                        CleanupError::Active => {
                            tr("The application is running again; no lock was removed.")
                        }
                        CleanupError::Changed => {
                            tr("The profile lock changed; no lock was removed. Scan again.")
                        }
                        other => other.to_string(),
                    };
                    state.toast_overlay.add_toast(adw::Toast::new(&message));
                    panel.refresh(&state);
                }
            }
        });
    }
}

fn removal_button() -> gtk::Button {
    let button = gtk::Button::with_label(&tr("Remove"));
    button.add_css_class("destructive-action");
    button.set_valign(Align::Center);
    button
}

impl CatalogList {
    fn new() -> Self {
        let list = gtk::ListBox::builder()
            .selection_mode(SelectionMode::None)
            .hexpand(true)
            .vexpand(true)
            .css_classes(["boxed-list"])
            .build();
        let viewport = adw::Clamp::builder()
            .maximum_size(720)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .child(&list)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .vexpand(true)
            .child(&viewport)
            .build();
        Self { scroller, list }
    }

    fn widget(&self) -> &gtk::ScrolledWindow {
        &self.scroller
    }

    fn clear(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
    }

    fn build_row<F>(&self, icon_name: &str, title: &str, subtitle: &str, activate: F) -> gtk::Widget
    where
        F: Fn() + 'static,
    {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .title_lines(1)
            .subtitle_lines(1)
            .activatable(true)
            .selectable(false)
            .build();
        let icon = gtk::Image::from_icon_name(icon_name);
        icon.set_pixel_size(40);
        icon.set_valign(Align::Center);
        row.add_prefix(&icon);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        row.set_tooltip_text(Some(title));
        row.connect_activated(move |_| activate());
        row.upcast()
    }
}

#[derive(Clone)]
struct ProcessPanel {
    root: gtk::Box,
    list: gtk::ListBox,
    state: AppState,
    application: InstalledApplication,
    generation: Rc<Cell<u64>>,
    force_eligible: Rc<RefCell<HashSet<ProcessIdentity>>>,
}

#[derive(Clone)]
struct DiagnosisView {
    panel: gtk::Box,
    title: gtk::Label,
    explanation: gtk::Label,
    next_step: gtk::Label,
}

impl DiagnosisView {
    fn new() -> Self {
        let panel = gtk::Box::new(Orientation::Vertical, 6);
        panel.set_margin_top(12);
        panel.set_margin_bottom(8);
        panel.set_margin_start(12);
        panel.set_margin_end(12);
        panel.add_css_class("diagnostic-issue");
        panel.set_visible(false);

        let eyebrow = gtk::Label::builder()
            .label(tr("Problem detected"))
            .xalign(0.0)
            .build();
        eyebrow.add_css_class("caption-heading");
        let title = gtk::Label::builder().xalign(0.0).wrap(true).build();
        title.add_css_class("heading");
        let explanation = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        let next_step = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        next_step.add_css_class("dim-label");
        panel.append(&eyebrow);
        panel.append(&title);
        panel.append(&explanation);
        panel.append(&next_step);
        Self {
            panel,
            title,
            explanation,
            next_step,
        }
    }

    fn show(&self, diagnosis: &LaunchDiagnosisState) {
        let Some(copy) = launch_diagnosis_copy(diagnosis) else {
            return;
        };
        self.title.set_label(&copy.title);
        self.explanation.set_label(&copy.explanation);
        self.next_step.set_label(&copy.next_step);
        self.panel.set_visible(true);
    }
}

#[derive(Default)]
struct LaunchDiagnosisState {
    application_name: String,
    problem: Option<LaunchProblem>,
    references: HashMap<u32, ReferencedProcess>,
    failure_confirmed: bool,
    failed_exit_code: Option<i32>,
    failed_signal: Option<i32>,
}

struct LaunchDiagnosisCopy {
    title: String,
    explanation: String,
    next_step: String,
}

fn diagnosis_and_log_text(diagnosis: &LaunchDiagnosisState, log: &str) -> String {
    let Some(copy) = launch_diagnosis_copy(diagnosis) else {
        return log.to_owned();
    };
    format!(
        "{}\n{}: {}\n\n{}\n{}\n{}\n\n{}\n{}",
        tr("Launch diagnosis"),
        tr("Application"),
        diagnosis.application_name,
        copy.title,
        copy.explanation,
        copy.next_step,
        tr("Technical log"),
        log,
    )
}

fn launch_diagnosis_copy(diagnosis: &LaunchDiagnosisState) -> Option<LaunchDiagnosisCopy> {
    if !diagnosis.failure_confirmed {
        return None;
    }
    let application = &diagnosis.application_name;
    let Some(problem) = diagnosis.problem else {
        let explanation = if let Some(code) = diagnosis.failed_exit_code {
            tr("{application} stopped with exit code {code}, but Duck Packages could not identify the cause from its output.")
                .replace("{application}", application)
                .replace("{code}", &code.to_string())
        } else if let Some(signal) = diagnosis.failed_signal {
            tr("{application} was stopped by signal {signal}, but Duck Packages could not identify the cause from its output.")
                .replace("{application}", application)
                .replace("{signal}", &signal.to_string())
        } else {
            tr("{application} could not be started, but Duck Packages could not identify the cause.")
                .replace("{application}", application)
        };
        return Some(LaunchDiagnosisCopy {
            title: tr("The application did not open"),
            explanation,
            next_step: tr("Review the technical log below for more details."),
        });
    };
    match problem {
        LaunchProblem::ProfileLocked { referenced_pid } => {
            let reference = referenced_pid
                .and_then(|pid| diagnosis.references.get(&pid).map(|state| (pid, *state)));
            let explanation = match reference {
                Some((pid, ReferencedProcess::Missing)) => tr(
                    "{application} did not open because its profile contains a stale lock referencing PID {pid}, which is no longer running. The application stopped to avoid corrupting its data.",
                )
                .replace("{application}", application)
                .replace("{pid}", &pid.to_string()),
                Some((pid, ReferencedProcess::Verified(_))) => tr(
                    "{application} did not open because PID {pid} is actively using the same profile.",
                )
                .replace("{application}", application)
                .replace("{pid}", &pid.to_string()),
                Some((pid, ReferencedProcess::Possible(_) | ReferencedProcess::Unrelated(_))) => tr(
                    "{application} reports that PID {pid} is using the same profile, but Duck Packages could not confirm that the process belongs to this application.",
                )
                .replace("{application}", application)
                .replace("{pid}", &pid.to_string()),
                Some((pid, ReferencedProcess::Unreadable)) => tr(
                    "{application} reports that PID {pid} is using the same profile. Duck Packages could not inspect that process.",
                )
                .replace("{application}", application)
                .replace("{pid}", &pid.to_string()),
                None => tr(
                    "{application} did not open because its profile is locked, usually because another process is using it. The application stopped to avoid corrupting its data.",
                )
                .replace("{application}", application),
            };
            let next_step = match reference {
                Some((_, ReferencedProcess::Missing)) => tr(
                    "Next step: close every {application} window. If no related process is running, remove the stale profile lock and try again.",
                )
                .replace("{application}", application),
                Some((_, ReferencedProcess::Verified(_))) => tr(
                    "Next step: close the existing {application} session normally, then try again.",
                )
                .replace("{application}", application),
                _ => tr(
                    "Next step: close every {application} window you recognize, then try again. Do not remove the lock while a related process is running.",
                )
                .replace("{application}", application),
            };
            Some(LaunchDiagnosisCopy {
                title: tr("The application profile is locked"),
                explanation,
                next_step,
            })
        }
        LaunchProblem::PermissionDenied => Some(LaunchDiagnosisCopy {
            title: tr("Permission denied"),
            explanation: tr(
                "{application} could not access a file or system resource required to start.",
            )
            .replace("{application}", application),
            next_step: tr(
                "Next step: check the ownership and permissions of the file named in the technical log.",
            ),
        }),
        LaunchProblem::RequiredFileMissing => Some(LaunchDiagnosisCopy {
            title: tr("A required file or library is missing"),
            explanation: tr(
                "{application} could not find a file or shared library required to start.",
            )
            .replace("{application}", application),
            next_step: tr(
                "Next step: repair or reinstall the application package, then try again.",
            ),
        }),
        LaunchProblem::GraphicalSessionUnavailable => Some(LaunchDiagnosisCopy {
            title: tr("The graphical session is unavailable"),
            explanation: tr(
                "{application} could not connect to the display server for this desktop session.",
            )
            .replace("{application}", application),
            next_step: tr(
                "Next step: open the application from an active graphical session and check the display settings in the technical log.",
            ),
        }),
    }
}

impl ProcessPanel {
    fn new(state: &AppState, application: &InstalledApplication) -> Self {
        let root = gtk::Box::new(Orientation::Vertical, 8);
        root.set_margin_start(24);
        root.set_margin_end(24);

        let header = gtk::Box::new(Orientation::Horizontal, 8);
        let title = gtk::Label::builder()
            .label(tr("Processes"))
            .xalign(0.0)
            .hexpand(true)
            .build();
        title.add_css_class("heading");
        let refresh_label = tr("Refresh processes");
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(&refresh_label)
            .css_classes(["flat"])
            .build();
        refresh.update_property(&[gtk::accessible::Property::Label(&refresh_label)]);
        header.append(&title);
        header.append(&refresh);

        let list = gtk::ListBox::builder()
            .selection_mode(SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        root.append(&header);
        root.append(&list);

        let panel = Self {
            root,
            list,
            state: state.clone(),
            application: application.clone(),
            generation: Rc::new(Cell::new(0)),
            force_eligible: Rc::new(RefCell::new(HashSet::new())),
        };
        let panel_for_refresh = panel.clone();
        refresh.connect_clicked(move |_| panel_for_refresh.refresh());
        panel.refresh();
        panel
    }

    fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn refresh(&self) {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.show_loading();

        let application = self.application.clone();
        let mut known = tracked_processes(&self.state, &application);
        known.extend(self.force_eligible.borrow().iter().copied());
        let panel = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result =
                blocking::unblock(move || processes::scan_application(&application, &known)).await;
            if panel.generation.get() != generation {
                return;
            }
            match result {
                Ok(process_state) => panel.render(process_state),
                Err(error) => panel.show_error(&error.to_string()),
            }
        });
    }

    fn refresh_after_launch(&self, pid: Option<u32>) {
        if let Some(pid) = pid {
            track_process(&self.state, &self.application, pid);
        }
        self.refresh();
        let panel = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(750), move || {
            panel.refresh();
        });
    }

    fn show_loading(&self) {
        clear_list_box(&self.list);
        let row = adw::ActionRow::builder()
            .title(tr("Checking running processes…"))
            .build();
        let spinner = gtk::Spinner::builder().spinning(true).build();
        row.add_prefix(&spinner);
        self.list.append(&row);
    }

    fn show_error(&self, message: &str) {
        clear_list_box(&self.list);
        let row = adw::ActionRow::builder()
            .title(tr("Could not inspect processes"))
            .subtitle(message)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        self.list.append(&row);
    }

    fn render(&self, process_state: ApplicationProcessState) {
        clear_list_box(&self.list);
        let active: HashSet<_> = process_state
            .sessions
            .iter()
            .flat_map(ApplicationSession::identities)
            .collect();
        self.force_eligible
            .borrow_mut()
            .retain(|identity| active.contains(identity));

        if process_state.is_empty() {
            let row = adw::ActionRow::builder()
                .title(tr("Not running"))
                .subtitle(tr("No related processes were found."))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("media-playback-stop-symbolic"));
            self.list.append(&row);
            return;
        }

        for session in process_state.sessions {
            self.append_session(&session);
        }
        if !process_state.possible.is_empty() {
            let heading = adw::ActionRow::builder()
                .title(tr("Possible processes"))
                .subtitle(tr("These matches cannot be safely managed."))
                .build();
            heading.add_css_class("dim-label");
            self.list.append(&heading);
            for process in process_state.possible {
                let subtitle = tr("Possible process · PID {pid}")
                    .replace("{pid}", &process.identity.pid.to_string());
                let row = adw::ActionRow::builder()
                    .title(process.executable)
                    .subtitle(subtitle)
                    .build();
                row.add_prefix(&gtk::Image::from_icon_name("dialog-question-symbolic"));
                self.list.append(&row);
            }
        }
    }

    fn append_session(&self, session: &ApplicationSession) {
        let count = session.process_count();
        let count_text = trn("{count} process", "{count} processes", count as u32)
            .replace("{count}", &count.to_string());
        let title =
            tr("Session PID {pid}").replace("{pid}", &session.root.identity.pid.to_string());
        let row = adw::ExpanderRow::builder()
            .title(title)
            .subtitle(count_text)
            .build();

        let can_force = session
            .identities()
            .iter()
            .any(|identity| self.force_eligible.borrow().contains(identity));
        let (label, tooltip, mode) = if can_force {
            (
                tr("Force Quit"),
                tr("Force quit this session"),
                TerminationMode::Force,
            )
        } else {
            (tr("End"), tr("End this session"), TerminationMode::Graceful)
        };
        let terminate = gtk::Button::builder()
            .label(label)
            .tooltip_text(tooltip)
            .valign(Align::Center)
            .build();
        if mode == TerminationMode::Force {
            terminate.add_css_class("destructive-action");
        }
        let panel = self.clone();
        let session_for_action = session.clone();
        terminate.connect_clicked(move |_| {
            panel.confirm_termination(session_for_action.clone(), mode);
        });
        row.add_suffix(&terminate);
        append_process_node(&row, &session.root, 0);
        self.list.append(&row);
    }

    fn confirm_termination(&self, session: ApplicationSession, mode: TerminationMode) {
        let count = session.process_count();
        let count_text = trn("{count} process", "{count} processes", count as u32)
            .replace("{count}", &count.to_string());
        let (heading, body, response) = match mode {
            TerminationMode::Graceful => (
                tr("End this session?"),
                tr("{application} may have unsaved work. {count} will receive a request to exit.")
                    .replace("{application}", &self.application.display_name)
                    .replace("{count}", &count_text),
                tr("End"),
            ),
            TerminationMode::Force => (
                tr("Force quit this session?"),
                tr("{application} will stop immediately. Unsaved work in {count} may be lost.")
                    .replace("{application}", &self.application.display_name)
                    .replace("{count}", &count_text),
                tr("Force Quit"),
            ),
        };
        let dialog = adw::AlertDialog::builder()
            .heading(heading)
            .body(body)
            .close_response("cancel")
            .default_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", &tr("Cancel")), ("terminate", &response)]);
        dialog.set_response_appearance("terminate", adw::ResponseAppearance::Destructive);
        let panel = self.clone();
        dialog.choose(
            Some(&self.state.window),
            None::<&gio::Cancellable>,
            move |choice| {
                if choice == "terminate" {
                    panel.execute_termination(session, mode);
                }
            },
        );
    }

    fn execute_termination(&self, session: ApplicationSession, mode: TerminationMode) {
        let application = self.application.clone();
        let mut known = tracked_processes(&self.state, &application);
        known.extend(self.force_eligible.borrow().iter().copied());
        let panel = self.clone();
        let identities = session.identities();
        glib::MainContext::default().spawn_local(async move {
            let result = blocking::unblock(move || {
                processes::terminate_session(&application, &known, &session, mode)
            })
            .await;
            match result {
                Ok(report) if report.signaled == 0 && report.failed == 0 => {
                    panel
                        .state
                        .toast_overlay
                        .add_toast(adw::Toast::new(&tr("The session is no longer running.")));
                    panel.refresh();
                }
                Ok(report) => {
                    if mode == TerminationMode::Graceful {
                        panel.force_eligible.borrow_mut().extend(identities);
                    } else {
                        for identity in identities {
                            panel.force_eligible.borrow_mut().remove(&identity);
                        }
                    }
                    let message = if report.failed == 0 {
                        tr("Exit request sent.")
                    } else {
                        tr("Exit request sent, but some processes could not be reached.")
                    };
                    panel
                        .state
                        .toast_overlay
                        .add_toast(adw::Toast::new(&message));
                    let delay = if mode == TerminationMode::Graceful {
                        std::time::Duration::from_secs(2)
                    } else {
                        std::time::Duration::from_millis(350)
                    };
                    let panel_for_refresh = panel.clone();
                    glib::timeout_add_local_once(delay, move || panel_for_refresh.refresh());
                }
                Err(error) => {
                    panel
                        .state
                        .toast_overlay
                        .add_toast(adw::Toast::new(&error.to_string()));
                    panel.refresh();
                }
            }
        });
    }
}

fn clear_list_box(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn append_process_node(row: &adw::ExpanderRow, node: &ProcessNode, depth: u32) {
    let role = match node.role {
        ProcessRole::Main => tr("Main process"),
        ProcessRole::Child => tr("Child process"),
    };
    let subtitle = format!("{role} · PID {}", node.identity.pid);
    let child = adw::ActionRow::builder()
        .title(&node.executable)
        .subtitle(subtitle)
        .margin_start((depth * 12) as i32)
        .build();
    row.add_row(&child);
    for descendant in &node.children {
        append_process_node(row, descendant, depth + 1);
    }
}

fn tracked_processes(state: &AppState, application: &InstalledApplication) -> Vec<ProcessIdentity> {
    state
        .tracked_processes
        .borrow()
        .get(&application.desktop_id)
        .cloned()
        .unwrap_or_default()
}

fn track_process(state: &AppState, application: &InstalledApplication, pid: u32) {
    let Some(identity) = processes::identify(pid) else {
        return;
    };
    let mut tracked = state.tracked_processes.borrow_mut();
    let identities = tracked.entry(application.desktop_id.clone()).or_default();
    if !identities.contains(&identity) {
        identities.push(identity);
    }
}

pub fn build(application: &adw::Application) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Duck Packages")
        .default_width(980)
        .default_height(720)
        .width_request(360)
        .build();

    let navigation = adw::NavigationView::builder().pop_on_escape(true).build();
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&navigation));
    window.set_content(Some(&toast_overlay));

    let loading = loading_page();
    let root = adw::NavigationPage::new(&loading, "Duck Packages");
    root.set_tag(Some("root"));
    navigation.add(&root);

    let window_action = gio::SimpleAction::new("search", None);
    window.add_action(&window_action);
    let close_action = gio::SimpleAction::new("close", None);
    let window_for_close = window.clone();
    close_action.connect_activate(move |_, _| window_for_close.close());
    window.add_action(&close_action);

    let application = application.clone();
    let window_weak = window.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let backend = BackendFactory::detect().await;
        let capabilities = backend.capabilities().await;
        let applications = catalog::load_applications().await;
        if let Some(window) = window_weak.upgrade() {
            construct_root(&application, &window, backend, capabilities, applications);
        }
    });

    window
}

fn loading_page() -> gtk::Widget {
    let status = adw::StatusPage::builder()
        .title(tr("Loading installed applications…"))
        .description("Duck Packages")
        .icon_name("io.github.srwalkerb.DuckPackages-symbolic")
        .build();
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .halign(Align::Center)
        .build();
    status.set_child(Some(&spinner));
    status.upcast()
}

fn construct_root(
    application: &adw::Application,
    window: &adw::ApplicationWindow,
    backend: Arc<dyn PackageBackend>,
    capabilities: BackendCapabilities,
    applications: Vec<InstalledApplication>,
) {
    let navigation = window
        .content()
        .and_downcast::<adw::ToastOverlay>()
        .and_then(|overlay| overlay.child())
        .and_downcast::<adw::NavigationView>()
        .expect("navigation view");
    let toast_overlay = window
        .content()
        .and_downcast::<adw::ToastOverlay>()
        .expect("toast overlay");

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Duck Packages", "");
    header.set_title_widget(Some(&title));

    let menu = gio::Menu::new();
    menu.append(Some(&tr("About Duck Packages")), Some("app.about"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text(tr("Main menu"))
        .build();
    header.pack_end(&menu_button);
    toolbar.add_top_bar(&header);

    let search = gtk::SearchEntry::builder()
        .placeholder_text(tr("Search installed software"))
        .hexpand(true)
        .max_width_chars(48)
        .build();
    let search_clamp = adw::Clamp::builder()
        .maximum_size(720)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .child(&search)
        .build();

    let view_stack = adw::ViewStack::new();
    let apps_list = CatalogList::new();
    let apps_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    let profile_lock_panel = ProfileLockPanel::new();
    let apps_content = gtk::Box::new(Orientation::Vertical, 0);
    apps_content.append(profile_lock_panel.widget());
    apps_content.append(apps_list.widget());
    apps_stack.add_named(&apps_content, Some("content"));
    apps_stack.add_named(&empty_state(&tr("No applications found")), Some("empty"));
    let packages_list = CatalogList::new();
    let packages_stack = packages_stack(&packages_list);

    view_stack.add_titled_with_icon(
        &apps_stack,
        Some("apps"),
        &tr("Applications"),
        "view-list-symbolic",
    );
    view_stack.add_titled_with_icon(
        &packages_stack,
        Some("packages"),
        &tr("Packages"),
        "package-x-generic-symbolic",
    );
    let switcher = adw::ViewSwitcher::builder()
        .policy(adw::ViewSwitcherPolicy::Wide)
        .stack(&view_stack)
        .build();
    header.set_title_widget(Some(&switcher));

    let content = gtk::Box::new(Orientation::Vertical, 0);
    content.append(&search_clamp);
    if capabilities.kind == BackendKind::Diagnostic {
        content.append(&diagnostic_banner(&capabilities));
    }
    content.append(&view_stack);
    toolbar.set_content(Some(&content));

    let state = AppState {
        window: window.clone(),
        navigation: navigation.clone(),
        toast_overlay,
        search: search.clone(),
        apps_list,
        apps_stack,
        profile_lock_panel,
        packages_list,
        packages_stack,
        view_stack: view_stack.clone(),
        search_generation: Rc::new(Cell::new(0)),
        backend,
        capabilities,
        applications: Rc::new(RefCell::new(applications)),
        packages: Rc::new(RefCell::new(Vec::new())),
        tracked_processes: Rc::new(RefCell::new(HashMap::new())),
    };
    refresh_visible_list(&state);
    state.profile_lock_panel.refresh(&state);

    let state_for_search = state.clone();
    let search_generation = state.search_generation.clone();
    let search_timeout = Rc::new(RefCell::new(None::<glib::SourceId>));
    search.connect_search_changed(move |_| {
        if let Some(source) = search_timeout.borrow_mut().take() {
            source.remove();
        }
        let generation = search_generation.get().wrapping_add(1);
        search_generation.set(generation);
        let search_generation_for_task = search_generation.clone();
        let state_for_search = state_for_search.clone();
        let search_timeout_for_task = search_timeout.clone();
        *search_timeout.borrow_mut() = Some(glib::timeout_add_local_once(
            std::time::Duration::from_millis(150),
            move || {
                *search_timeout_for_task.borrow_mut() = None;
                if search_generation_for_task.get() == generation {
                    refresh_visible_list(&state_for_search);
                }
            },
        ));
    });

    let state_for_packages = state.clone();
    view_stack.connect_visible_child_name_notify(move |stack| {
        if stack.visible_child_name().as_deref() == Some("packages")
            && state_for_packages.packages.borrow().is_empty()
        {
            load_packages(&state_for_packages);
            return;
        }
        if stack.visible_child_name().as_deref() == Some("apps") {
            state_for_packages
                .profile_lock_panel
                .refresh(&state_for_packages);
        }
        refresh_visible_list(&state_for_packages);
    });
    let state_for_delayed_profile_refresh = state.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(750), move || {
        state_for_delayed_profile_refresh
            .profile_lock_panel
            .refresh(&state_for_delayed_profile_refresh);
    });
    let page = adw::NavigationPage::new(&toolbar, "Duck Packages");
    page.set_tag(Some("home"));
    navigation.replace(&[page]);

    install_about_action(application, window);
    let search_action = window
        .lookup_action("search")
        .and_downcast::<gio::SimpleAction>()
        .expect("search action");
    let search_for_action = search.clone();
    search_action.connect_activate(move |_, _| {
        search_for_action.grab_focus();
    });
}

fn empty_state(title: &str) -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title(title)
        .description(tr("Try a different search."))
        .build()
}

fn diagnostic_banner(capabilities: &BackendCapabilities) -> adw::Banner {
    let unavailable = tr("PackageKit is unavailable");
    let detail = capabilities.diagnostic.as_deref().unwrap_or(&unavailable);
    adw::Banner::builder()
        .title(format!(
            "{} — {} ({detail})",
            tr("Package management unavailable"),
            tr("Applications remain visible, but removal is disabled. Install or enable PackageKit 1.3.5 or newer.")
        ))
        .revealed(true)
        .build()
}

fn profile_lock_application_name(state: &AppState, lock: &StaleProfileLock) -> String {
    let profile = normalize_identifier(&lock.profile_name());
    state
        .applications
        .borrow()
        .iter()
        .find(|application| {
            [
                Some(application.display_name.as_str()),
                Some(application.desktop_id.as_str()),
                application.startup_wm_class.as_deref(),
                application
                    .executable
                    .as_deref()
                    .and_then(|path| std::path::Path::new(path).file_name())
                    .and_then(|name| name.to_str()),
            ]
            .into_iter()
            .flatten()
            .map(normalize_identifier)
            .any(|name| !name.is_empty() && (name.contains(&profile) || profile.contains(&name)))
        })
        .map(|application| application.display_name.clone())
        .unwrap_or_else(|| lock.profile_name())
}

fn normalize_identifier(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

const CATALOG_CHUNK_SIZE: usize = 40;

fn refresh_visible_list(state: &AppState) {
    let generation = state.search_generation.get().wrapping_add(1);
    state.search_generation.set(generation);
    if state.view_stack.visible_child_name().as_deref() == Some("packages") {
        if state.packages.borrow().is_empty() {
            return;
        }
        refresh_package_list(state, generation);
    } else {
        refresh_application_list(state, generation);
    }
}

fn refresh_application_list(state: &AppState, generation: u64) {
    let query = state.search.text().to_lowercase();
    let mut matches = Vec::new();
    for application in state.applications.borrow().iter() {
        if query.is_empty() || application.searchable_text().contains(&query) {
            matches.push(application.clone());
        }
    }
    let state_for_rows = state.clone();
    render_catalog_chunked(
        &state.apps_list,
        &state.apps_stack,
        &state.search_generation,
        generation,
        matches,
        move |application| {
            let state_for_activation = state_for_rows.clone();
            let application_for_activation = application.clone();
            state_for_rows.apps_list.build_row(
                &application.icon_name,
                &application.display_name,
                application.summary.as_deref().unwrap_or(""),
                move || {
                    open_application_details(
                        &state_for_activation,
                        application_for_activation.clone(),
                    );
                },
            )
        },
    );
}

fn render_catalog_chunked<T, B>(
    list: &CatalogList,
    stack: &gtk::Stack,
    generation: &Rc<Cell<u64>>,
    generation_number: u64,
    items: Vec<T>,
    build_row: B,
) where
    T: 'static,
    B: FnMut(T) -> gtk::Widget + 'static,
{
    let list = list.clone();
    let generation = generation.clone();
    let queue = items.into_iter().collect::<VecDeque<_>>();
    list.clear();
    stack.set_visible_child_name(if queue.is_empty() { "empty" } else { "content" });
    append_catalog_chunk(list, generation, generation_number, queue, build_row);
}

fn append_catalog_chunk<T, B>(
    list: CatalogList,
    generation: Rc<Cell<u64>>,
    generation_number: u64,
    mut queue: VecDeque<T>,
    mut build_row: B,
) where
    T: 'static,
    B: FnMut(T) -> gtk::Widget + 'static,
{
    let mut processed = 0;
    while processed < CATALOG_CHUNK_SIZE {
        let Some(item) = queue.pop_front() else {
            break;
        };
        let widget = build_row(item);
        list.list.append(&widget);
        processed += 1;
    }
    if queue.is_empty() {
        return;
    }
    let next_list = list.clone();
    let next_generation = generation.clone();
    glib::idle_add_local_once(move || {
        if next_generation.get() != generation_number {
            return;
        }
        append_catalog_chunk(
            next_list,
            next_generation,
            generation_number,
            queue,
            build_row,
        );
    });
}

fn open_application_details(state: &AppState, mut application: InstalledApplication) {
    let state = state.clone();
    let path = application.desktop_file.clone();
    glib::MainContext::default().spawn_local(async move {
        if application.owner.is_none() {
            if let Some(path) = path {
                application.owner = state.backend.find_owner(&path).await.ok().flatten();
            }
        }
        let package = if let Some(owner) = application.owner.clone() {
            state
                .backend
                .get_details(&[owner])
                .await
                .ok()
                .and_then(|packages| packages.into_iter().next())
        } else {
            None
        };
        let page = application_details_page(&state, &application, package.as_ref());
        state.navigation.push(&page);
    });
}

fn application_details_page(
    state: &AppState,
    application: &InstalledApplication,
    package: Option<&InstalledPackage>,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_show_back_button(true);
    toolbar.add_top_bar(&header);

    let hero = gtk::Box::new(Orientation::Horizontal, 24);
    hero.add_css_class("details-hero");
    let icon = gtk::Image::from_icon_name(&application.icon_name);
    icon.set_pixel_size(96);
    icon.set_valign(Align::Start);
    let identity = gtk::Box::new(Orientation::Vertical, 6);
    identity.set_hexpand(true);
    let title = gtk::Label::builder()
        .label(&application.display_name)
        .halign(Align::Start)
        .xalign(0.0)
        .wrap(true)
        .build();
    title.add_css_class("title-1");
    let summary = gtk::Label::builder()
        .label(application.summary.as_deref().unwrap_or(""))
        .halign(Align::Start)
        .xalign(0.0)
        .wrap(true)
        .build();
    summary.add_css_class("dim-label");
    identity.append(&title);
    identity.append(&summary);

    let process_panel = ProcessPanel::new(state, application);
    let actions = gtk::Box::new(Orientation::Horizontal, 8);
    actions.set_valign(Align::Center);
    let open = adw::SplitButton::builder()
        .label(tr("Open"))
        .dropdown_tooltip(tr("More opening options"))
        .build();
    open.add_css_class("suggested-action");
    let application_for_open = application.clone();
    let state_for_open = state.clone();
    let process_panel_for_open = process_panel.clone();
    open.connect_clicked(move |_| match catalog::launch(&application_for_open) {
        Ok(pid) => process_panel_for_open.refresh_after_launch(pid),
        Err(error) => state_for_open
            .toast_overlay
            .add_toast(adw::Toast::new(&error.to_string())),
    });
    let diagnostic_action = gio::SimpleAction::new("open-with-logs", None);
    let state_for_diagnostic = state.clone();
    let application_for_diagnostic = application.clone();
    let process_panel_for_diagnostic = process_panel.clone();
    diagnostic_action.connect_activate(move |_, _| {
        show_diagnostic_logs(
            &state_for_diagnostic,
            &application_for_diagnostic,
            &process_panel_for_diagnostic,
        );
    });
    let diagnostic_actions = gio::SimpleActionGroup::new();
    diagnostic_actions.add_action(&diagnostic_action);
    open.insert_action_group("details", Some(&diagnostic_actions));
    let open_menu = gio::Menu::new();
    open_menu.append(Some(&tr("Open with Logs")), Some("details.open-with-logs"));
    open.set_menu_model(Some(&open_menu));
    actions.append(&open);
    let remove = removal_button();
    remove.set_sensitive(state.capabilities.can_remove && package.is_some());
    if let Some(package) = package.cloned() {
        let state_for_remove = state.clone();
        let application_name = application.display_name.clone();
        remove.connect_clicked(move |_| {
            request_removal(
                &state_for_remove,
                package.clone(),
                vec![application_name.clone()],
            );
        });
    }
    actions.append(&remove);
    hero.append(&icon);
    hero.append(&identity);
    hero.append(&actions);

    let content = gtk::Box::new(Orientation::Vertical, 18);
    content.set_margin_bottom(32);
    content.append(&hero);
    if let Some(package) = package {
        content.append(&package_properties(package));
    }
    content.append(process_panel.widget());
    if let Some(description) = application.description.as_deref() {
        let description_label = gtk::Label::builder()
            .label(description)
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .margin_start(24)
            .margin_end(24)
            .build();
        content.append(&description_label);
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(840)
        .child(&content)
        .build();
    let scrolled = gtk::ScrolledWindow::builder().child(&clamp).build();
    let responsive = adw::BreakpointBin::builder()
        .child(&scrolled)
        .width_request(0)
        .height_request(0)
        .build();
    let compact = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        500.0,
        adw::LengthUnit::Sp,
    ));
    compact.add_setter(
        &hero,
        "orientation",
        Some(&Orientation::Vertical.to_value()),
    );
    compact.add_setter(&actions, "halign", Some(&Align::Start.to_value()));
    compact.add_setter(&icon, "pixel-size", Some(&72_i32.to_value()));
    responsive.add_breakpoint(compact);
    toolbar.set_content(Some(&responsive));
    adw::NavigationPage::new(&toolbar, &application.display_name)
}

fn show_diagnostic_logs(
    state: &AppState,
    application: &InstalledApplication,
    detail_process_panel: &ProcessPanel,
) {
    let log_window = adw::Window::builder()
        .title(format!("{} — {}", tr("Logs"), application.display_name))
        .default_width(760)
        .default_height(520)
        .width_request(360)
        .transient_for(&state.window)
        .destroy_with_parent(true)
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new(&tr("Diagnostic Logs"), &application.display_name);
    header.set_title_widget(Some(&title));

    let buffer = gtk::TextBuffer::new(None);
    let toast_overlay = adw::ToastOverlay::new();
    let diagnosis_state = Rc::new(RefCell::new(LaunchDiagnosisState {
        application_name: application.display_name.clone(),
        ..Default::default()
    }));

    let copy_label = tr("Copy");
    let copy = gtk::MenuButton::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text(&copy_label)
        .build();
    copy.update_property(&[gtk::accessible::Property::Label(&copy_label)]);
    let copy_popover = gtk::Popover::new();
    let copy_options = gtk::Box::new(Orientation::Vertical, 2);
    copy_options.set_margin_top(6);
    copy_options.set_margin_bottom(6);
    copy_options.set_margin_start(6);
    copy_options.set_margin_end(6);

    let copy_log = gtk::Button::with_label(&tr("Copy Technical Log"));
    copy_log.set_halign(Align::Fill);
    copy_log.add_css_class("flat");
    let buffer_for_log_copy = buffer.clone();
    let toast_overlay_for_log_copy = toast_overlay.clone();
    let popover_for_log_copy = copy_popover.clone();
    copy_log.connect_clicked(move |button| {
        let log = buffer_for_log_copy.text(
            &buffer_for_log_copy.start_iter(),
            &buffer_for_log_copy.end_iter(),
            false,
        );
        button.clipboard().set_text(&log);
        popover_for_log_copy.popdown();
        toast_overlay_for_log_copy.add_toast(adw::Toast::new(&tr("Technical log copied")));
    });

    let copy_diagnosis = gtk::Button::with_label(&tr("Copy Diagnosis and Log"));
    copy_diagnosis.set_halign(Align::Fill);
    copy_diagnosis.add_css_class("flat");
    let buffer_for_diagnosis_copy = buffer.clone();
    let diagnosis_state_for_copy = diagnosis_state.clone();
    let toast_overlay_for_diagnosis_copy = toast_overlay.clone();
    let popover_for_diagnosis_copy = copy_popover.clone();
    copy_diagnosis.connect_clicked(move |button| {
        let log = buffer_for_diagnosis_copy.text(
            &buffer_for_diagnosis_copy.start_iter(),
            &buffer_for_diagnosis_copy.end_iter(),
            false,
        );
        let text = diagnosis_and_log_text(&diagnosis_state_for_copy.borrow(), &log);
        button.clipboard().set_text(&text);
        popover_for_diagnosis_copy.popdown();
        toast_overlay_for_diagnosis_copy
            .add_toast(adw::Toast::new(&tr("Diagnosis and log copied")));
    });
    copy_options.append(&copy_log);
    copy_options.append(&copy_diagnosis);
    copy_popover.set_child(Some(&copy_options));
    copy.set_popover(Some(&copy_popover));
    header.pack_end(&copy);
    let processes_label = tr("Processes");
    let processes = gtk::Button::builder()
        .icon_name("system-run-symbolic")
        .tooltip_text(&processes_label)
        .build();
    processes.update_property(&[gtk::accessible::Property::Label(&processes_label)]);
    let state_for_processes = state.clone();
    let application_for_processes = application.clone();
    processes.connect_clicked(move |_| {
        show_process_window(&state_for_processes, &application_for_processes);
    });
    header.pack_end(&processes);
    toolbar.add_top_bar(&header);

    let status = gtk::Label::builder()
        .label(tr("Launching…"))
        .xalign(0.0)
        .wrap(true)
        .margin_top(12)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    status.add_css_class("dim-label");
    let diagnosis_view = DiagnosisView::new();

    let text_view = gtk::TextView::builder()
        .buffer(&buffer)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Automatic)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .child(&text_view)
        .build();
    scrolled.add_css_class("card");

    let content = gtk::Box::new(Orientation::Vertical, 0);
    content.append(&diagnosis_view.panel);
    content.append(&status);
    content.append(&scrolled);
    toolbar.set_content(Some(&content));
    toast_overlay.set_child(Some(&toolbar));
    log_window.set_content(Some(&toast_overlay));

    let launcher = application
        .desktop_file
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| application.desktop_id.clone());
    let executable = application.executable.as_deref().unwrap_or("—");
    append_unbounded(
        &buffer,
        &format!(
            "[{}] Duck Packages\n{}: {}\n{}: {}\n\n",
            diagnostic_timestamp(),
            tr("Desktop launcher"),
            launcher,
            tr("Executable"),
            executable,
        ),
    );

    let closed = Rc::new(Cell::new(false));
    let closed_for_window = closed.clone();
    log_window.connect_close_request(move |_| {
        closed_for_window.set(true);
        glib::Propagation::Proceed
    });
    log_window.present();

    let buffer_weak = buffer.downgrade();
    let status_weak = status.downgrade();
    let adjustment_weak = scrolled.vadjustment().downgrade();
    let application = application.clone();
    let state = state.clone();
    let detail_process_panel = detail_process_panel.clone();
    let diagnosis_view_for_session = diagnosis_view.clone();
    let diagnosis_state_for_session = diagnosis_state.clone();
    glib::MainContext::default().spawn_local(async move {
        let session = match catalog::launch_with_logs(&application) {
            Ok(session) => session,
            Err(error) => {
                {
                    let mut diagnosis = diagnosis_state_for_session.borrow_mut();
                    diagnosis.failure_confirmed = true;
                    diagnosis.problem = launch_diagnostic::analyze_text(&error.to_string());
                }
                diagnosis_view_for_session.show(&diagnosis_state_for_session.borrow());
                if !closed.get() {
                    if let Some(status) = status_weak.upgrade() {
                        status.set_label(&tr("Launch failed"));
                        status.add_css_class("error");
                    }
                    if let Some(buffer) = buffer_weak.upgrade() {
                        append_unbounded(
                            &buffer,
                            &format!(
                                "[{}] [Duck Packages] {}: {}\n",
                                diagnostic_timestamp(),
                                tr("Launch failed"),
                                error
                            ),
                        );
                    }
                }
                return;
            }
        };

        let retained = Rc::new(Cell::new(buffer_weak.upgrade().map_or(0, |buffer| {
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .len()
        })));
        let truncated = Rc::new(Cell::new(false));
        let saw_output = Rc::new(Cell::new(false));
        let no_output_notice = Rc::new(Cell::new(false));
        let mut stdout_pid_scanner = PidReferenceScanner::default();
        let mut stderr_pid_scanner = PidReferenceScanner::default();
        let mut problem_analyzer = LaunchProblemAnalyzer::default();
        let mut seen_referenced_pids = HashSet::new();
        let saw_output_for_notice = saw_output.clone();
        let no_output_notice_for_timeout = no_output_notice.clone();
        let closed_for_notice = closed.clone();
        let buffer_for_notice = buffer_weak.clone();
        let adjustment_for_notice = adjustment_weak.clone();
        glib::timeout_add_seconds_local_once(3, move || {
            if closed_for_notice.get() || saw_output_for_notice.get() {
                return;
            }
            no_output_notice_for_timeout.set(true);
            if let Some(buffer) = buffer_for_notice.upgrade() {
                append_unbounded(&buffer, &no_output_message());
            }
            if let Some(adjustment) = adjustment_for_notice.upgrade() {
                scroll_to_bottom(&adjustment);
            }
        });
        let events = session.events;
        while let Ok(event) = events.recv().await {
            if closed.get() {
                continue;
            }
            let Some(buffer) = buffer_weak.upgrade() else {
                continue;
            };
            let was_at_bottom = adjustment_weak
                .upgrade()
                .is_none_or(|adjustment| adjustment_is_at_bottom(&adjustment));
            let (line, status_text) = diagnostic_event_text(&event);
            let referenced_pids = match &event {
                catalog::DiagnosticEvent::Output { stream, bytes } => {
                    for problem in problem_analyzer.push(bytes) {
                        let mut diagnosis = diagnosis_state_for_session.borrow_mut();
                        if diagnosis.problem.is_none() {
                            diagnosis.problem = Some(problem);
                        }
                    }
                    match stream {
                        catalog::DiagnosticStream::Stdout => stdout_pid_scanner.push(bytes),
                        catalog::DiagnosticStream::Stderr => stderr_pid_scanner.push(bytes),
                    }
                }
                _ => Vec::new(),
            };
            match &event {
                catalog::DiagnosticEvent::ProcessStarted(pid) => {
                    track_process(&state, &application, *pid);
                    detail_process_panel.refresh_after_launch(Some(*pid));
                }
                catalog::DiagnosticEvent::ProcessExited { code, signal } => {
                    if code.is_some_and(|code| code != 0) || signal.is_some() {
                        let mut diagnosis = diagnosis_state_for_session.borrow_mut();
                        diagnosis.failure_confirmed = true;
                        diagnosis.failed_exit_code = code.filter(|code| *code != 0);
                        diagnosis.failed_signal = *signal;
                        drop(diagnosis);
                        diagnosis_view_for_session.show(&diagnosis_state_for_session.borrow());
                    }
                    detail_process_panel.refresh();
                }
                _ => {}
            }
            if matches!(event, catalog::DiagnosticEvent::Output { .. }) {
                saw_output.set(true);
            }
            if let Some(status_text) = status_text
                && let Some(status) = status_weak.upgrade()
            {
                status.set_label(&status_text);
            }
            if append_limited(&buffer, &line, &retained, &truncated) {
                append_unbounded(
                    &buffer,
                    &format!(
                        "\n[{}] [Duck Packages] {}\n",
                        diagnostic_timestamp(),
                        tr("Output limit reached. Additional output is not being kept.")
                    ),
                );
            }
            for pid in referenced_pids {
                if seen_referenced_pids.insert(pid) {
                    inspect_referenced_process(
                        &state,
                        &application,
                        pid,
                        &buffer_weak,
                        &adjustment_weak,
                        &closed,
                        &retained,
                        &truncated,
                        &diagnosis_state_for_session,
                        &diagnosis_view_for_session,
                    );
                }
            }
            if was_at_bottom && let Some(adjustment) = adjustment_weak.upgrade() {
                glib::idle_add_local_once(move || scroll_to_bottom(&adjustment));
            }
        }
        for problem in problem_analyzer.finish() {
            let mut diagnosis = diagnosis_state_for_session.borrow_mut();
            if diagnosis.problem.is_none() {
                diagnosis.problem = Some(problem);
            }
        }
        diagnosis_view_for_session.show(&diagnosis_state_for_session.borrow());
        for pid in stdout_pid_scanner
            .finish()
            .into_iter()
            .chain(stderr_pid_scanner.finish())
        {
            if seen_referenced_pids.insert(pid) {
                inspect_referenced_process(
                    &state,
                    &application,
                    pid,
                    &buffer_weak,
                    &adjustment_weak,
                    &closed,
                    &retained,
                    &truncated,
                    &diagnosis_state_for_session,
                    &diagnosis_view_for_session,
                );
            }
        }
        if !closed.get()
            && !saw_output.get()
            && !no_output_notice.get()
            && let Some(buffer) = buffer_weak.upgrade()
        {
            append_limited(&buffer, &no_output_message(), &retained, &truncated);
            if let Some(adjustment) = adjustment_weak.upgrade() {
                scroll_to_bottom(&adjustment);
            }
        }
    });
}

#[derive(Default)]
struct PidReferenceScanner {
    partial: String,
}

impl PidReferenceScanner {
    fn push(&mut self, bytes: &[u8]) -> Vec<u32> {
        self.partial.push_str(&String::from_utf8_lossy(bytes));
        let mut pids = Vec::new();
        while let Some(newline) = self.partial.find('\n') {
            let line = self.partial[..newline].to_owned();
            self.partial.drain(..=newline);
            pids.extend(launch_diagnostic::extract_pid_references(&line));
        }
        if self.partial.len() > 16 * 1024 {
            pids.extend(launch_diagnostic::extract_pid_references(&self.partial));
            self.partial.clear();
        }
        pids
    }

    fn finish(&mut self) -> Vec<u32> {
        let pids = launch_diagnostic::extract_pid_references(&self.partial);
        self.partial.clear();
        pids
    }
}

#[allow(clippy::too_many_arguments)]
fn inspect_referenced_process(
    state: &AppState,
    application: &InstalledApplication,
    pid: u32,
    buffer: &glib::WeakRef<gtk::TextBuffer>,
    adjustment: &glib::WeakRef<gtk::Adjustment>,
    closed: &Rc<Cell<bool>>,
    retained: &Rc<Cell<usize>>,
    truncated: &Rc<Cell<bool>>,
    diagnosis_state: &Rc<RefCell<LaunchDiagnosisState>>,
    diagnosis_view: &DiagnosisView,
) {
    let application_for_scan = application.clone();
    let known = tracked_processes(state, application);
    let buffer = buffer.clone();
    let adjustment = adjustment.clone();
    let closed = closed.clone();
    let retained = retained.clone();
    let truncated = truncated.clone();
    let diagnosis_state = diagnosis_state.clone();
    let diagnosis_view = diagnosis_view.clone();
    glib::MainContext::default().spawn_local(async move {
        let result = blocking::unblock(move || {
            processes::inspect_referenced_pid(&application_for_scan, &known, pid)
        })
        .await;
        if closed.get() {
            return;
        }
        let message = referenced_process_message(pid, result);
        diagnosis_state.borrow_mut().references.insert(pid, result);
        diagnosis_view.show(&diagnosis_state.borrow());
        if let Some(buffer) = buffer.upgrade() {
            append_limited(&buffer, &message, &retained, &truncated);
        }
        if let Some(adjustment) = adjustment.upgrade() {
            scroll_to_bottom(&adjustment);
        }
    });
}

fn referenced_process_message(pid: u32, result: ReferencedProcess) -> String {
    let message = match result {
        ReferencedProcess::Missing => {
            tr("Referenced PID {pid} is not active. The reference may be stale.")
        }
        ReferencedProcess::Verified(_) => {
            tr("Referenced PID {pid} belongs to a verified application session.")
        }
        ReferencedProcess::Possible(_) => {
            tr("Referenced PID {pid} may belong to this application, but cannot be safely managed.")
        }
        ReferencedProcess::Unrelated(_) => {
            tr("Referenced PID {pid} is active, but could not be associated with this application.")
        }
        ReferencedProcess::Unreadable => tr("Referenced PID {pid} could not be inspected."),
    }
    .replace("{pid}", &pid.to_string());
    format!("[{}] [Duck Packages] {message}\n", diagnostic_timestamp())
}

fn show_process_window(state: &AppState, application: &InstalledApplication) {
    let window = adw::Window::builder()
        .title(format!(
            "{} — {}",
            tr("Processes"),
            application.display_name
        ))
        .default_width(620)
        .default_height(520)
        .width_request(360)
        .transient_for(&state.window)
        .destroy_with_parent(true)
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &tr("Processes"),
        &application.display_name,
    )));
    toolbar.add_top_bar(&header);
    let panel = ProcessPanel::new(state, application);
    panel.widget().set_margin_top(18);
    panel.widget().set_margin_bottom(24);
    toolbar.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .child(panel.widget())
            .build(),
    ));
    window.set_content(Some(&toolbar));
    window.present();
}

fn no_output_message() -> String {
    format!(
        "[{}] [Duck Packages] {}\n",
        diagnostic_timestamp(),
        tr(
            "No output has been captured yet. The app may use D-Bus activation, reuse an existing process, or not write to standard output."
        )
    )
}

fn diagnostic_timestamp() -> String {
    glib::DateTime::now_local()
        .and_then(|time| time.format("%Y-%m-%d %H:%M:%S"))
        .map(|time| time.to_string())
        .unwrap_or_else(|_| "—".into())
}

fn diagnostic_event_text(event: &catalog::DiagnosticEvent) -> (String, Option<String>) {
    let timestamp = diagnostic_timestamp();
    match event {
        catalog::DiagnosticEvent::ProcessStarted(pid) => {
            let status = tr("Process started (PID {pid})").replace("{pid}", &pid.to_string());
            (
                format!("[{timestamp}] [Duck Packages] {status}\n"),
                Some(status),
            )
        }
        catalog::DiagnosticEvent::ProcessIdUnavailable => {
            let status = tr("Launched (PID unavailable)");
            (
                format!("[{timestamp}] [Duck Packages] {status}\n"),
                Some(status),
            )
        }
        catalog::DiagnosticEvent::Output { stream, bytes } => (
            format_diagnostic_output(&timestamp, stream.label(), bytes),
            None,
        ),
        catalog::DiagnosticEvent::StreamFailed { stream, message } => (
            format!(
                "[{timestamp}] [Duck Packages] {} ({}): {message}\n",
                tr("Could not read output"),
                stream.label()
            ),
            None,
        ),
        catalog::DiagnosticEvent::ProcessExited { code, signal } => {
            let status = match (code, signal) {
                (Some(0), _) => tr("Process exited normally."),
                (Some(code), _) => tr("The application did not open (exit code {code}).")
                    .replace("{code}", &code.to_string()),
                (_, Some(signal)) => tr("Process ended with signal {signal}.")
                    .replace("{signal}", &signal.to_string()),
                _ => tr("Process exited."),
            };
            (
                format!("[{timestamp}] [Duck Packages] {status}\n"),
                Some(status),
            )
        }
    }
}

fn format_diagnostic_output(timestamp: &str, stream: &str, bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut formatted = String::new();
    for line in text.split_inclusive('\n') {
        formatted.push_str(&format!("[{timestamp}] [{stream}] {line}"));
    }
    if !text.ends_with('\n') {
        formatted.push('\n');
    }
    formatted
}

fn append_limited(
    buffer: &gtk::TextBuffer,
    text: &str,
    retained: &Cell<usize>,
    truncated: &Cell<bool>,
) -> bool {
    if truncated.get() {
        return false;
    }
    let keep = retained_prefix_len(retained.get(), text);
    if keep > 0 {
        append_unbounded(buffer, &text[..keep]);
        retained.set(retained.get() + keep);
    }
    let reached_limit = keep < text.len();
    if reached_limit && !truncated.replace(true) {
        return true;
    }
    false
}

fn retained_prefix_len(current_size: usize, text: &str) -> usize {
    valid_prefix_len(text, DIAGNOSTIC_LOG_LIMIT.saturating_sub(current_size))
}

fn valid_prefix_len(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn append_unbounded(buffer: &gtk::TextBuffer, text: &str) {
    buffer.insert(&mut buffer.end_iter(), text);
}

fn adjustment_is_at_bottom(adjustment: &gtk::Adjustment) -> bool {
    adjustment.value() + adjustment.page_size() >= adjustment.upper() - 4.0
}

fn scroll_to_bottom(adjustment: &gtk::Adjustment) {
    adjustment.set_value((adjustment.upper() - adjustment.page_size()).max(adjustment.lower()));
}

fn package_properties(package: &InstalledPackage) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(tr("Package"))
        .margin_start(24)
        .margin_end(24)
        .build();
    for (title, value) in [
        (tr("Package"), package.name.clone()),
        (tr("Version"), package.version.clone()),
        (tr("Architecture"), package.arch.clone()),
        (
            tr("Origin"),
            package.origin.clone().unwrap_or_else(|| "—".into()),
        ),
        (tr("Installed size"), format_size(package.installed_size)),
    ] {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(value)
            .build();
        group.add(&row);
    }
    group
}

fn package_details_page(state: &AppState, package: &InstalledPackage) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_show_back_button(true);
    toolbar.add_top_bar(&header);

    let content = gtk::Box::new(Orientation::Vertical, 18);
    content.set_margin_top(32);
    content.set_margin_bottom(32);
    content.set_margin_start(24);
    content.set_margin_end(24);
    let hero = gtk::Box::new(Orientation::Horizontal, 20);
    let icon = gtk::Image::from_icon_name("package-x-generic-symbolic");
    icon.set_pixel_size(72);
    let text = gtk::Box::new(Orientation::Vertical, 6);
    text.set_hexpand(true);
    let title = gtk::Label::builder()
        .label(&package.name)
        .xalign(0.0)
        .wrap(true)
        .build();
    title.add_css_class("title-1");
    let summary = gtk::Label::builder()
        .label(package.summary.as_deref().unwrap_or(""))
        .xalign(0.0)
        .wrap(true)
        .build();
    summary.add_css_class("dim-label");
    text.append(&title);
    text.append(&summary);
    let remove = removal_button();
    remove.set_sensitive(state.capabilities.can_remove);
    let state_for_remove = state.clone();
    let package_for_remove = package.clone();
    remove.connect_clicked(move |_| {
        request_removal(&state_for_remove, package_for_remove.clone(), Vec::new());
    });
    hero.append(&icon);
    hero.append(&text);
    hero.append(&remove);
    content.append(&hero);
    content.append(&package_properties(package));
    if let Some(description) = package.description.as_deref() {
        content.append(
            &gtk::Label::builder()
                .label(description)
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .build(),
        );
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(840)
        .child(&content)
        .build();
    toolbar.set_content(Some(&gtk::ScrolledWindow::builder().child(&clamp).build()));
    adw::NavigationPage::new(&toolbar, &package.name)
}

fn request_removal(state: &AppState, package: InstalledPackage, affected: Vec<String>) {
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let request = match RemovalRequest::safe(vec![package.id.clone()]) {
            Ok(request) => request,
            Err(error) => return show_error(&state, &error),
        };
        match state.backend.simulate_removal(request).await {
            Ok(mut plan) => {
                plan.affected_applications = affected;
                show_removal_plan(&state, plan);
            }
            Err(error) => show_error(&state, &error),
        }
    });
}

fn show_removal_plan(state: &AppState, plan: RemovalPlan) {
    let package_names = if plan.packages_to_remove.is_empty() {
        plan.request
            .package_ids
            .iter()
            .map(|id| id.fields().0.to_owned())
            .collect::<Vec<_>>()
    } else {
        plan.packages_to_remove
            .iter()
            .map(|package| package.name.clone())
            .collect::<Vec<_>>()
    };
    let package_count = package_names.len();
    let restart = match plan.restart {
        RestartRequirement::None => None,
        RestartRequirement::Application => Some(tr("The application must be restarted.")),
        RestartRequirement::Session => Some(tr("Your session must be restarted.")),
        RestartRequirement::System => Some(tr("The computer must be restarted.")),
    };
    let package_count_text = trn("{count} package", "{count} packages", package_count as u32)
        .replace("{count}", &package_count.to_string());
    let mut body = format!(
        "{}\n\n{}\n{} {}",
        tr("Review every package that will be removed before continuing."),
        package_count_text,
        format_size(plan.estimated_freed_bytes),
        tr("freed")
    );
    if !plan.affected_applications.is_empty() {
        body.push_str(&format!(
            "\n\n{}\n{}",
            tr("Affected applications"),
            plan.affected_applications.join(", ")
        ));
    }
    if let Some(restart) = restart {
        body.push_str(&format!("\n\n{restart}"));
    }
    let list = gtk::Box::new(Orientation::Vertical, 4);
    list.add_css_class("impact-panel");
    for name in package_names.iter().take(12) {
        let row = gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        list.append(&row);
    }
    if package_count > 12 {
        let remaining = package_count - 12;
        let more = trn("+ {count} more", "+ {count} more", remaining as u32)
            .replace("{count}", &remaining.to_string());
        list.append(&gtk::Label::new(Some(&more)));
    }
    let dialog = adw::AlertDialog::builder()
        .heading(tr("Removal impact"))
        .body(body)
        .extra_child(&list)
        .close_response("cancel")
        .default_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", &tr("Cancel")), ("remove", &tr("Remove"))]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    let state = state.clone();
    let parent = state.window.clone();
    dialog.choose(Some(&parent), None::<&gio::Cancellable>, move |response| {
        if response == "remove" {
            execute_removal(&state, plan);
        }
    });
}

fn execute_removal(state: &AppState, plan: RemovalPlan) {
    let progress = adw::AlertDialog::builder()
        .heading(tr("Removing…"))
        .body(tr("Waiting for authorization…"))
        .can_close(false)
        .build();
    let bar = gtk::ProgressBar::new();
    bar.set_show_text(true);
    progress.set_extra_child(Some(&bar));
    progress.present(Some(&state.window));

    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        match state.backend.remove(plan).await {
            Ok(mut events) => {
                while let Some(event) = events.next().await {
                    match event {
                        TransactionEvent::WaitingForAuthorization => {
                            progress.set_body(&tr("Waiting for authorization…"));
                        }
                        TransactionEvent::Started => progress.set_body(&tr("Removing…")),
                        TransactionEvent::PackageProgress { id, percentage } => {
                            progress.set_body(id.fields().0);
                            bar.set_fraction(percentage as f64 / 100.0);
                        }
                        TransactionEvent::OverallProgress(percentage) => {
                            bar.set_fraction(percentage as f64 / 100.0);
                        }
                        TransactionEvent::RestartRequired(_) => {}
                        TransactionEvent::Completed => {
                            progress.force_close();
                            state
                                .toast_overlay
                                .add_toast(adw::Toast::new(&tr("Removed")));
                            state.navigation.pop_to_tag("home");
                            reload(&state);
                        }
                        TransactionEvent::Failed(error) => {
                            progress.force_close();
                            show_error(&state, &error);
                        }
                    }
                }
            }
            Err(error) => {
                progress.force_close();
                show_error(&state, &error);
            }
        }
    });
}

fn reload(state: &AppState) {
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        *state.applications.borrow_mut() = catalog::load_applications().await;
        *state.packages.borrow_mut() = Vec::new();
        if state.view_stack.visible_child_name().as_deref() == Some("packages") {
            load_packages(&state);
        } else {
            refresh_visible_list(&state);
        }
        state.profile_lock_panel.refresh(&state);
    });
}

fn show_error(state: &AppState, error: &BackendError) {
    let dialog = adw::AlertDialog::new(Some("Duck Packages"), Some(&error.to_string()));
    dialog.add_response("close", &tr("Close"));
    dialog.set_close_response("close");
    dialog.present(Some(&state.window));
}

fn packages_stack(list: &CatalogList) -> gtk::Stack {
    let notice = adw::Banner::builder()
        .title(tr("Libraries and system components are shown here. Removing them can affect other software."))
        .revealed(true)
        .build();
    let content = gtk::Box::new(Orientation::Vertical, 0);
    content.append(&notice);
    content.append(list.widget());
    let spinner = adw::StatusPage::builder()
        .title(tr("Loading installed packages…"))
        .description(tr("Advanced package view"))
        .icon_name("package-x-generic-symbolic")
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&content, Some("content"));
    stack.add_named(&spinner, Some("loading"));
    stack.add_named(&empty_state(&tr("No packages found")), Some("empty"));
    stack.set_visible_child_name("loading");
    stack
}

fn load_packages(state: &AppState) {
    state.packages_stack.set_visible_child_name("loading");
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        match state.backend.list_installed().await {
            Ok(mut packages) => {
                packages.sort_by_key(|package| package.name.to_lowercase());
                *state.packages.borrow_mut() = packages;
                let generation = state.search_generation.get().wrapping_add(1);
                state.search_generation.set(generation);
                refresh_package_list(&state, generation);
            }
            Err(error) => {
                state.packages_stack.set_visible_child_name("empty");
                show_error(&state, &error);
            }
        }
    });
}

fn refresh_package_list(state: &AppState, generation: u64) {
    let query = state.search.text().to_lowercase();
    let mut matches = Vec::new();
    for package in state.packages.borrow().iter() {
        if query.is_empty() || package.searchable_text().contains(&query) {
            matches.push(package.clone());
        }
    }
    let state_for_rows = state.clone();
    render_catalog_chunked(
        &state.packages_list,
        &state.packages_stack,
        &state.search_generation,
        generation,
        matches,
        move |package| {
            let state_for_activation = state_for_rows.clone();
            let package_for_activation = package.clone();
            let subtitle = format!(
                "{} · {} · {}",
                package.version,
                package.arch,
                format_size(package.installed_size)
            );
            state_for_rows.packages_list.build_row(
                "package-x-generic-symbolic",
                &package.name,
                &subtitle,
                move || {
                    state_for_activation.navigation.push(&package_details_page(
                        &state_for_activation,
                        &package_for_activation,
                    ));
                },
            )
        },
    );
}

fn install_about_action(application: &adw::Application, window: &adw::ApplicationWindow) {
    if application.lookup_action("about").is_some() {
        return;
    }
    let action = gio::SimpleAction::new("about", None);
    let window = window.clone();
    action.connect_activate(move |_, _| {
        let about = adw::AboutDialog::builder()
            .application_name("Duck Packages")
            .application_icon("io.github.srwalkerb.DuckPackages")
            .version(version())
            .developer_name("Duck Packages contributors")
            .license_type(gtk::License::Gpl30)
            .website("https://github.com/SrWalkerB/duck-manager")
            .issue_url("https://github.com/SrWalkerB/duck-manager/issues")
            .build();
        about.present(Some(&window));
    });
    application.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_prefix_never_splits_a_character() {
        assert_eq!(valid_prefix_len("abc🦆", 5), 3);
        assert_eq!(valid_prefix_len("abc🦆", 7), 7);
    }

    #[test]
    fn diagnostic_exit_event_includes_the_code() {
        let (line, status) = diagnostic_event_text(&catalog::DiagnosticEvent::ProcessExited {
            code: Some(42),
            signal: None,
        });
        assert!(line.contains("42"));
        assert!(status.is_some_and(|status| status.contains("42")));
    }

    #[test]
    fn every_diagnostic_output_line_has_a_stream_label() {
        let output = format_diagnostic_output("now", "stderr", b"first\nsecond\n");
        assert_eq!(output, "[now] [stderr] first\n[now] [stderr] second\n");
    }

    #[test]
    fn diagnostic_log_never_retains_more_than_one_mebibyte() {
        assert_eq!(retained_prefix_len(DIAGNOSTIC_LOG_LIMIT - 2, "abcd"), 2);
        assert_eq!(retained_prefix_len(DIAGNOSTIC_LOG_LIMIT, "abcd"), 0);
    }

    #[test]
    fn pid_references_require_an_explicit_process_marker() {
        assert_eq!(
            launch_diagnostic::extract_pid_references("another process (18956) owns the profile",),
            vec![18956]
        );
        assert_eq!(
            launch_diagnostic::extract_pid_references("pid=42 failed"),
            vec![42]
        );
        assert_eq!(
            launch_diagnostic::extract_pid_references("error 21 at 18956 bytes"),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn pid_scanner_handles_lines_split_between_output_chunks() {
        let mut scanner = PidReferenceScanner::default();
        assert!(scanner.push(b"profile owned by pro").is_empty());
        assert_eq!(scanner.push(b"cess (18956)\n").as_slice(), &[18956]);
        assert!(scanner.finish().is_empty());
    }

    #[test]
    fn referenced_pid_messages_do_not_expose_command_arguments() {
        let message = referenced_process_message(42, ReferencedProcess::Missing);
        assert!(message.contains("42"));
        assert!(!message.contains("--"));
    }

    #[test]
    fn stale_profile_lock_uses_the_selected_application_name() {
        let mut diagnosis = LaunchDiagnosisState {
            application_name: "Example Browser".into(),
            problem: Some(LaunchProblem::ProfileLocked {
                referenced_pid: Some(18956),
            }),
            failure_confirmed: true,
            failed_exit_code: Some(21),
            ..Default::default()
        };
        diagnosis
            .references
            .insert(18956, ReferencedProcess::Missing);
        let copy = launch_diagnosis_copy(&diagnosis).unwrap();
        assert!(copy.title.contains("locked"));
        assert!(copy.explanation.contains("18956"));
        assert!(copy.explanation.contains("Example Browser"));
        assert!(copy.explanation.contains("stale"));
        assert!(copy.next_step.contains("remove"));
        assert!(!copy.explanation.contains("Chrome"));
    }

    #[test]
    fn unknown_nonzero_exit_does_not_invent_a_cause() {
        let diagnosis = LaunchDiagnosisState {
            application_name: "Example Editor".into(),
            failure_confirmed: true,
            failed_exit_code: Some(7),
            ..Default::default()
        };
        let copy = launch_diagnosis_copy(&diagnosis).unwrap();
        assert!(copy.explanation.contains('7'));
        assert!(copy.explanation.contains("Example Editor"));
        assert!(copy.explanation.contains("could not identify"));
    }

    #[test]
    fn generic_failure_categories_produce_actionable_diagnoses() {
        let diagnosis = LaunchDiagnosisState {
            application_name: "Example Editor".into(),
            problem: Some(LaunchProblem::RequiredFileMissing),
            failure_confirmed: true,
            ..Default::default()
        };
        let copy = launch_diagnosis_copy(&diagnosis).unwrap();
        assert!(copy.title.contains("missing"));
        assert!(copy.explanation.contains("Example Editor"));
        assert!(copy.next_step.contains("reinstall"));
    }

    #[test]
    fn an_error_hint_is_not_shown_before_failure_is_confirmed() {
        let diagnosis = LaunchDiagnosisState {
            application_name: "Example Editor".into(),
            problem: Some(LaunchProblem::PermissionDenied),
            ..Default::default()
        };
        assert!(launch_diagnosis_copy(&diagnosis).is_none());
    }

    #[test]
    fn combined_copy_contains_the_diagnosis_and_technical_log() {
        let diagnosis = LaunchDiagnosisState {
            application_name: "Example Editor".into(),
            problem: Some(LaunchProblem::PermissionDenied),
            failure_confirmed: true,
            ..Default::default()
        };
        let text = diagnosis_and_log_text(&diagnosis, "[stderr] Permission denied");
        assert!(text.contains("Example Editor"));
        assert!(text.contains("Permission denied"));
        assert!(text.contains("Technical log"));
        assert!(text.contains("[stderr] Permission denied"));
    }

    #[test]
    fn combined_copy_falls_back_to_the_log_while_diagnosis_is_pending() {
        let diagnosis = LaunchDiagnosisState {
            application_name: "Example Editor".into(),
            ..Default::default()
        };
        assert_eq!(diagnosis_and_log_text(&diagnosis, "launching"), "launching");
    }
}
