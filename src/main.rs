mod backend;
mod catalog;
mod domain;
mod ui;

use adw::prelude::*;
use gettextrs::{LocaleCategory, bindtextdomain, gettext, ngettext, setlocale, textdomain};

const APP_ID: &str = "io.github.srwalkerb.DuckPackages";
const GETTEXT_PACKAGE: &str = "duck-packages";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> glib::ExitCode {
    init_localization();
    gio::resources_register_include!("duck-packages.gresource")
        .expect("failed to register application resources");

    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_startup(|_| load_css());
    application.connect_activate(|application| {
        if let Some(window) = application.active_window() {
            window.present();
            return;
        }
        ui::build(application).present();
    });
    application.set_accels_for_action("win.search", &["<Control>f"]);
    application.set_accels_for_action("win.close", &["<Control>w"]);
    application.run()
}

fn init_localization() {
    setlocale(LocaleCategory::LcAll, "");
    let locale_dir = option_env!("DUCK_PACKAGES_LOCALEDIR").unwrap_or("/usr/share/locale");
    bindtextdomain(GETTEXT_PACKAGE, locale_dir).expect("failed to bind gettext domain");
    textdomain(GETTEXT_PACKAGE).expect("failed to select gettext domain");
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_resource("/io/github/srwalkerb/DuckPackages/style.css");
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("a graphical display is required"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

pub(crate) fn tr(message: &str) -> String {
    gettext(message)
}

pub(crate) fn trn(singular: &str, plural: &str, count: u32) -> String {
    ngettext(singular, plural, count)
}

pub(crate) fn version() -> &'static str {
    VERSION
}
