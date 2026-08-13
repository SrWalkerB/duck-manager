use std::{cell::RefCell, rc::Rc, sync::Arc};

use adw::prelude::*;
use futures_util::StreamExt;
use gtk::{Align, FlowBox, FlowBoxChild, Orientation, PolicyType, SelectionMode};

use crate::{
    backend::{BackendFactory, PackageBackend},
    catalog,
    domain::{
        BackendCapabilities, BackendError, BackendKind, InstalledApplication, InstalledPackage,
        RemovalPlan, RemovalRequest, RestartRequirement, TransactionEvent, format_size,
    },
    tr, trn, version,
};

#[derive(Clone)]
struct AppState {
    window: adw::ApplicationWindow,
    navigation: adw::NavigationView,
    toast_overlay: adw::ToastOverlay,
    search: gtk::SearchEntry,
    apps_flow: FlowBox,
    apps_stack: gtk::Stack,
    packages_store: gio::ListStore,
    packages_view: gtk::ColumnView,
    packages_stack: gtk::Stack,
    backend: Arc<dyn PackageBackend>,
    capabilities: BackendCapabilities,
    applications: Rc<RefCell<Vec<InstalledApplication>>>,
    packages: Rc<RefCell<Vec<InstalledPackage>>>,
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
    let apps_flow = FlowBox::builder()
        .selection_mode(SelectionMode::None)
        .homogeneous(true)
        .column_spacing(12)
        .row_spacing(12)
        .max_children_per_line(6)
        .min_children_per_line(1)
        .halign(Align::Fill)
        .valign(Align::Start)
        .hexpand(true)
        .vexpand(false)
        .build();
    let apps_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    apps_stack.add_named(&app_scroller(&apps_flow), Some("content"));
    apps_stack.add_named(&empty_state(), Some("empty"));
    let packages_store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let (packages_stack, packages_view) = packages_view(&packages_store);

    view_stack.add_titled_with_icon(
        &apps_stack,
        Some("apps"),
        &tr("Applications"),
        "view-grid-symbolic",
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
        apps_flow: apps_flow.clone(),
        apps_stack,
        packages_store,
        packages_view,
        packages_stack,
        backend,
        capabilities,
        applications: Rc::new(RefCell::new(applications)),
        packages: Rc::new(RefCell::new(Vec::new())),
    };
    refresh_application_grid(&state);

    let state_for_search = state.clone();
    search.connect_search_changed(move |_| {
        refresh_application_grid(&state_for_search);
        refresh_package_store(&state_for_search);
    });

    let state_for_packages = state.clone();
    view_stack.connect_visible_child_name_notify(move |stack| {
        if stack.visible_child_name().as_deref() == Some("packages")
            && state_for_packages.packages.borrow().is_empty()
        {
            load_packages(&state_for_packages);
        }
    });
    let state_for_package_activation = state.clone();
    state.packages_view.connect_activate(move |view, position| {
        let Some(model) = view.model() else {
            return;
        };
        let Some(object) = model.item(position).and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let package = object.borrow::<InstalledPackage>().clone();
        state_for_package_activation
            .navigation
            .push(&package_details_page(
                &state_for_package_activation,
                &package,
            ));
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

fn app_scroller(flow: &FlowBox) -> gtk::ScrolledWindow {
    let viewport = gtk::Box::new(Orientation::Vertical, 0);
    viewport.set_margin_top(12);
    viewport.set_margin_bottom(24);
    viewport.set_margin_start(24);
    viewport.set_margin_end(24);
    viewport.append(flow);
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vexpand(true)
        .child(&viewport)
        .build()
}

fn empty_state() -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title(tr("No applications found"))
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

fn refresh_application_grid(state: &AppState) {
    while let Some(child) = state.apps_flow.first_child() {
        state.apps_flow.remove(&child);
    }
    let query = state.search.text().to_lowercase();
    let applications: Vec<_> = state
        .applications
        .borrow()
        .iter()
        .filter(|application| query.is_empty() || application.searchable_text().contains(&query))
        .cloned()
        .collect();
    state
        .apps_stack
        .set_visible_child_name(if applications.is_empty() {
            "empty"
        } else {
            "content"
        });
    for application in applications {
        let child = FlowBoxChild::new();
        child.set_halign(Align::Start);
        child.set_valign(Align::Start);
        child.set_size_request(196, -1);
        child.set_child(Some(&application_card(&application)));
        child.set_focusable(true);
        child.set_tooltip_text(Some(&application.display_name));
        let state_for_click = state.clone();
        let application_for_click = application.clone();
        let gesture = gtk::GestureClick::new();
        gesture.connect_released(move |_, _, _, _| {
            open_application_details(&state_for_click, application_for_click.clone())
        });
        child.add_controller(gesture);
        let key = gtk::EventControllerKey::new();
        let state_for_key = state.clone();
        let application_for_key = application.clone();
        key.connect_key_released(move |_, key, _, _| {
            if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::space) {
                open_application_details(&state_for_key, application_for_key.clone());
            }
        });
        child.add_controller(key);
        state.apps_flow.insert(&child, -1);
    }
}

fn application_card(application: &InstalledApplication) -> gtk::Widget {
    let content = gtk::Box::new(Orientation::Vertical, 8);
    // GTK size requests describe the content box; CSS padding brings the card to 196×164.
    content.set_size_request(164, 132);
    content.set_valign(Align::Start);
    content.add_css_class("app-card");
    content.add_css_class("card");
    let icon = gtk::Image::from_icon_name(&application.icon_name);
    icon.set_pixel_size(64);
    icon.set_halign(Align::Start);
    let title = gtk::Label::builder()
        .label(&application.display_name)
        .halign(Align::Start)
        .xalign(0.0)
        .wrap(true)
        .lines(2)
        .max_width_chars(22)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    title.add_css_class("app-card-title");
    let summary = gtk::Label::builder()
        .label(application.summary.as_deref().unwrap_or(""))
        .halign(Align::Start)
        .xalign(0.0)
        .wrap(true)
        .lines(2)
        .max_width_chars(22)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    summary.add_css_class("app-card-summary");
    content.append(&icon);
    content.append(&title);
    content.append(&summary);
    content.upcast()
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

    let actions = gtk::Box::new(Orientation::Horizontal, 8);
    actions.set_valign(Align::Center);
    let open = gtk::Button::with_label(&tr("Open"));
    open.add_css_class("suggested-action");
    let application_for_open = application.clone();
    let state_for_open = state.clone();
    open.connect_clicked(move |_| {
        if let Err(error) = catalog::launch(&application_for_open) {
            state_for_open
                .toast_overlay
                .add_toast(adw::Toast::new(&error.to_string()));
        }
    });
    actions.append(&open);
    let remove = gtk::Button::with_label(&tr("Remove"));
    remove.add_css_class("destructive-action");
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
    toolbar.set_content(Some(&scrolled));
    adw::NavigationPage::new(&toolbar, &application.display_name)
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
    let remove = gtk::Button::with_label(&tr("Remove"));
    remove.add_css_class("destructive-action");
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
        refresh_application_grid(&state);
        refresh_package_store(&state);
    });
}

fn show_error(state: &AppState, error: &BackendError) {
    let dialog = adw::AlertDialog::new(Some("Duck Packages"), Some(&error.to_string()));
    dialog.add_response("close", &tr("Close"));
    dialog.set_close_response("close");
    dialog.present(Some(&state.window));
}

fn packages_view(store: &gio::ListStore) -> (gtk::Stack, gtk::ColumnView) {
    let view = gtk::ColumnView::new(None::<gtk::SelectionModel>);
    view.add_css_class("package-list");
    view.set_vexpand(true);
    let package_column = text_column(&tr("Package"), |package| package.name.clone(), true);
    package_column.set_sorter(Some(&package_sorter(|first, second| {
        first.name.to_lowercase().cmp(&second.name.to_lowercase())
    })));
    view.append_column(&package_column);
    view.append_column(&text_column(
        &tr("Version"),
        |package| package.version.clone(),
        true,
    ));
    view.append_column(&text_column(
        &tr("Architecture"),
        |package| package.arch.clone(),
        false,
    ));
    let size_column = text_column(
        &tr("Installed size"),
        |package| format_size(package.installed_size),
        false,
    );
    size_column.set_sorter(Some(&package_sorter(|first, second| {
        first.installed_size.cmp(&second.installed_size)
    })));
    view.append_column(&size_column);

    view.sort_by_column(Some(&package_column), gtk::SortType::Ascending);
    let sorted = gtk::SortListModel::new(Some(store.clone()), view.sorter());
    let sorted_for_view = sorted.clone();
    view.connect_sorter_notify(move |view| {
        sorted_for_view.set_sorter(view.sorter().as_ref());
    });
    let selection = gtk::SingleSelection::new(Some(sorted));
    view.set_model(Some(&selection));

    let notice = adw::Banner::builder()
        .title(tr("Libraries and system components are shown here. Removing them can affect other software."))
        .revealed(true)
        .build();
    let content = gtk::Box::new(Orientation::Vertical, 0);
    content.append(&notice);
    content.append(&gtk::ScrolledWindow::builder().child(&view).build());
    let spinner = adw::StatusPage::builder()
        .title(tr("Loading installed packages…"))
        .description(tr("Advanced package view"))
        .icon_name("package-x-generic-symbolic")
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(&content, Some("content"));
    stack.add_named(&spinner, Some("loading"));
    stack.add_named(&empty_state(), Some("empty"));
    stack.set_visible_child_name("loading");
    (stack, view)
}

fn package_sorter(
    compare: fn(&InstalledPackage, &InstalledPackage) -> std::cmp::Ordering,
) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |first, second| {
        let Some(first) = first.downcast_ref::<glib::BoxedAnyObject>() else {
            return gtk::Ordering::Equal;
        };
        let Some(second) = second.downcast_ref::<glib::BoxedAnyObject>() else {
            return gtk::Ordering::Equal;
        };
        match compare(
            &first.borrow::<InstalledPackage>(),
            &second.borrow::<InstalledPackage>(),
        ) {
            std::cmp::Ordering::Less => gtk::Ordering::Smaller,
            std::cmp::Ordering::Equal => gtk::Ordering::Equal,
            std::cmp::Ordering::Greater => gtk::Ordering::Larger,
        }
    })
}

fn text_column(
    title: &str,
    value: fn(&InstalledPackage) -> String,
    expand: bool,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("list item");
        let label = item.child().and_downcast::<gtk::Label>().expect("label");
        let object = item
            .item()
            .and_downcast::<glib::BoxedAnyObject>()
            .expect("package object");
        label.set_label(&value(&object.borrow::<InstalledPackage>()));
    });
    gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .expand(expand)
        .resizable(true)
        .build()
}

fn load_packages(state: &AppState) {
    state.packages_stack.set_visible_child_name("loading");
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        match state.backend.list_installed().await {
            Ok(mut packages) => {
                packages.sort_by_key(|package| package.name.to_lowercase());
                *state.packages.borrow_mut() = packages;
                refresh_package_store(&state);
            }
            Err(error) => {
                state.packages_stack.set_visible_child_name("empty");
                show_error(&state, &error);
            }
        }
    });
}

fn refresh_package_store(state: &AppState) {
    state.packages_store.remove_all();
    let query = state.search.text().to_lowercase();
    for package in state.packages.borrow().iter() {
        if query.is_empty() || package.searchable_text().contains(&query) {
            state
                .packages_store
                .append(&glib::BoxedAnyObject::new(package.clone()));
        }
    }
    state
        .packages_stack
        .set_visible_child_name(if state.packages_store.n_items() == 0 {
            "empty"
        } else {
            "content"
        });
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
            .website("https://github.com/srwalkerb/duck-package-manager")
            .issue_url("https://github.com/srwalkerb/duck-package-manager/issues")
            .build();
        about.present(Some(&window));
    });
    application.add_action(&action);
}
