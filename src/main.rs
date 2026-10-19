//! LinuxPods - AirPods management for GNOME.
//!
//! GTK owns the main thread; a multi-threaded tokio runtime carries the BLE, AAP
//! and D-Bus work, and the two meet over async channels consumed on the GTK main
//! context.

use linuxpods::{attachment, ble, indicator, podstate, ui};

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use bluer::Address;
use futures_util::StreamExt;
use gtk::{gio, glib};

use indicator::{Indicator, TrayActions};
use linuxpods::aap::NoiseMode;
use podstate::Coordinator;

const APP_ID: &str = "io.github.mstroecker.LinuxPods";

/// Bridges tray clicks onto the GTK main context, and noise control onto tokio.
struct AppActions {
    window: async_channel::Sender<WindowCommand>,
    coordinator: Arc<Coordinator>,
    /// Needed because ksni calls back from its own thread, which is not
    /// necessarily inside the runtime.
    runtime: tokio::runtime::Handle,
}

enum WindowCommand {
    Present,
    Quit,
}

impl TrayActions for AppActions {
    fn show_window(&self) {
        let _ = self.window.send_blocking(WindowCommand::Present);
    }

    fn quit(&self) {
        let _ = self.window.send_blocking(WindowCommand::Quit);
    }

    fn set_noise_mode(&self, mac: Address, mode: NoiseMode) {
        let coordinator = self.coordinator.clone();
        self.runtime.spawn(async move {
            if let Err(e) = coordinator.set_noise_control(mac, mode).await {
                tracing::warn!("failed to set noise control from tray: {e:#}");
            }
        });
    }
}

/// The option the autostart entry passes: start in the tray, window hidden.
const MINIMIZED: &str = "minimized";

/// Declares the command-line options. GApplication parses them in whichever
/// process was launched and hands them to the primary instance's
/// `command-line` - over D-Bus, when that is another process.
fn add_options(app: &impl IsA<gio::Application>) {
    app.add_main_option(
        MINIMIZED,
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Start in the system tray without showing the window",
        None,
    );
}

/// Whether a launch brings up the window. `--minimized` leaves it as it is:
/// hidden when the launch starts the app, and not raised when the app is
/// already running - autostart must not pop up a window the user put away.
fn shows_window(options: &glib::VariantDict) -> bool {
    !options.contains(MINIMIZED)
}

/// The half of the app only the primary instance runs. It is set up in
/// `startup`, which a second launch never reaches: that one registers, finds
/// the primary, hands it its command line and exits.
struct Primary {
    coordinator: Arc<Coordinator>,
    runtime: tokio::runtime::Handle,
    /// Tray commands, for the window once it is built.
    window_rx: async_channel::Receiver<WindowCommand>,
    /// Built by the first launch and kept: every later one lands in this
    /// instance, and the window from the first is the one to raise. Building a
    /// second would leave two windows on one coordinator.
    window: RefCell<Option<adw::ApplicationWindow>>,
}

impl Primary {
    /// Starts the background workers. Between them they reach well outside the
    /// process - BLE discovery, the BlueZ battery provider, an AAP link to every
    /// connected pair, the tray icon - so there must only ever be one set.
    fn start(coordinator: Arc<Coordinator>, runtime: tokio::runtime::Handle) -> Self {
        let (window_tx, window_rx) = async_channel::unbounded::<WindowCommand>();

        runtime.spawn(ble_task(coordinator.clone()));
        runtime.spawn(attachment::run(coordinator.clone()));
        runtime.spawn(expiry_task(coordinator.clone()));
        runtime.spawn(tray_task(
            coordinator.clone(),
            Arc::new(AppActions {
                window: window_tx,
                coordinator: coordinator.clone(),
                runtime: runtime.clone(),
            }),
        ));

        Self {
            coordinator,
            runtime,
            window_rx,
            window: RefCell::default(),
        }
    }

    /// The window, built on first use. A new one is not shown: whether it is
    /// shown is up to the caller.
    fn window(&self, app: &adw::Application) -> adw::ApplicationWindow {
        if let Some(win) = self.window.borrow().as_ref() {
            return win.clone();
        }

        let win = ui::activate(app, self.coordinator.clone(), self.runtime.clone());
        *self.window.borrow_mut() = Some(win.clone());

        // Tray commands arrive here, on the main context, where touching the
        // window is legal.
        let window_rx = self.window_rx.clone();
        let app = app.clone();
        let shown = win.clone();
        glib::spawn_future_local(async move {
            while let Ok(cmd) = window_rx.recv().await {
                match cmd {
                    WindowCommand::Present => shown.present(),
                    WindowCommand::Quit => app.quit(),
                }
            }
        });

        win
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "linuxpods=info".into()),
        )
        .init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("failed to start tokio runtime: {e}");
            return glib::ExitCode::FAILURE;
        }
    };

    // Handling the command line, rather than leaving GApplication to turn it into
    // an activation, is what carries a second launch's options to the running
    // instance - see `shows_window`.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    add_options(&app);

    // Closing the window only hides it (see ui::activate), so quitting is an
    // action of its own, for the main menu and Ctrl+Q.
    app.add_action_entries([gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.quit())
        .build()]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);

    // GApplication finds out inside `run` whether this process is the primary
    // instance, so nothing with an effect outside it may start before then.
    let primary: Rc<OnceCell<Primary>> = Rc::default();

    // UI automation for the test harness, on the connection GApplication
    // registered the app with: it does so before `startup`, which a second
    // launch never reaches, so only the primary instance serves it. Connected
    // first, so it runs first and is up before the window is built. It serves
    // only when the fake world's script asks for it - see
    // `ui::register_automation`.
    #[cfg(feature = "harness")]
    app.connect_startup(ui::register_automation);

    let started = primary.clone();
    let handle = runtime.handle().clone();
    app.connect_startup(move |_| {
        // The window's artwork and the About dialog's icon load from the bundle.
        ui::register_resources();

        let coordinator = match handle.block_on(Coordinator::new()) {
            Ok(c) => c,
            Err(e) => {
                // Nothing is started; the command line finds that and fails the run.
                tracing::error!("failed to create pod state coordinator: {e}");
                return;
            }
        };
        // Startup runs once, so the cell is still empty.
        let _ = started.set(Primary::start(coordinator, handle.clone()));
    });

    let launched = primary.clone();
    app.connect_command_line(move |app, cmdline| {
        // Empty only when startup failed, and said why.
        let Some(primary) = launched.get() else {
            return glib::ExitCode::FAILURE;
        };

        let shows = shows_window(&cmdline.options_dict());
        if cmdline.is_remote() {
            tracing::info!("launched again, minimized: {}", !shows);
        }

        if shows {
            app.activate();
        } else {
            // --minimized hands the app to the tray: the window is built, which is
            // what keeps GTK running, but stays hidden until something asks for it.
            primary.window(app);
        }
        glib::ExitCode::SUCCESS
    });

    // Runs for every launch without --minimized, the first and any later one -
    // from the launcher entry, say, while the app sits in the tray.
    app.connect_activate(move |app| {
        if let Some(primary) = primary.get() {
            primary.window(app).present();
        }
    });

    let code = app.run();
    // Keep the runtime alive until GTK returns.
    drop(runtime);
    code
}

/// Feeds BLE advertisements into the coordinator.
async fn ble_task(coordinator: Arc<Coordinator>) {
    let scanner = match ble::scanner::Scanner::new().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to create BLE scanner: {e}");
            return;
        }
    };

    if let Err(e) = scanner.start_discovery().await {
        tracing::error!("failed to start BLE discovery: {e}");
        return;
    }

    let stream = match scanner.advertisements().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to subscribe to advertisements: {e}");
            return;
        }
    };
    tokio::pin!(stream);

    while let Some(advert) = stream.next().await {
        coordinator
            .handle_advertisement(advert.data, advert.ble_mac)
            .await;
    }
}

/// Expires cached BLE readings on a clock. Advertisements prune as they arrive,
/// but with no device in range none arrive, and a cached reading would never go.
async fn expiry_task(coordinator: Arc<Coordinator>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tick.tick().await;
        coordinator.expire().await;
    }
}

/// Runs the system tray and keeps it in sync with coordinator state.
async fn tray_task(coordinator: Arc<Coordinator>, actions: Arc<AppActions>) {
    let handle = match Indicator::new(actions).start().await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("system tray unavailable: {e}");
            return;
        }
    };

    let mut updates = coordinator.subscribe();
    while updates.changed().await.is_ok() {
        // Cloned out: the borrow is a read lock every broadcast waits for.
        let snapshot = updates.borrow_and_update().clone();
        indicator::apply_snapshot(&handle, &snapshot).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Puts `args` through GApplication's own option parsing and reports what
    /// the primary instance's command line would do with the window.
    fn shows_window_for(args: &[&str]) -> bool {
        // NON_UNIQUE keeps the test off the session bus: the app is its own
        // primary and handles its command line itself, as the running instance
        // handles a forwarded one.
        let app = gio::Application::new(
            None,
            gio::ApplicationFlags::HANDLES_COMMAND_LINE | gio::ApplicationFlags::NON_UNIQUE,
        );
        add_options(&app);

        let shows = Rc::new(Cell::new(None));
        let seen = shows.clone();
        app.connect_command_line(move |_, cmdline| {
            seen.set(Some(shows_window(&cmdline.options_dict())));
            glib::ExitCode::SUCCESS
        });

        assert_eq!(app.run_with_args(args), glib::ExitCode::SUCCESS);
        shows.get().expect("the command line reached the handler")
    }

    /// The flag used to be filtered out of argv by hand. Registered with
    /// GApplication instead, it has to come back out of the options dict under
    /// the name `shows_window` looks up.
    #[test]
    fn minimized_keeps_the_window_hidden() {
        assert!(shows_window_for(&["linuxpods"]));
        assert!(!shows_window_for(&["linuxpods", "--minimized"]));
    }
}
