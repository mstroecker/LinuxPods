//! UI-layer spike: internal/ui/window.go ported to gtk4-rs + libadwaita-rs.

mod state;
mod ui;

use adw::prelude::*;

fn main() -> gtk::glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("dev.linuxpods.UiSpike")
        .build();

    app.connect_activate(|app| {
        let updates = state::spawn_mock_coordinator();
        ui::activate(app, updates);
    });

    app.run()
}
