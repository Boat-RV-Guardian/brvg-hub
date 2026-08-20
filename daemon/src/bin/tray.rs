//! `brvg-hub-tray` — the hub's presence in the notification area.
//!
//! The hub is a SYSTEM scheduled task with no window, which means the honest answer to "is my boat
//! being watched right now?" has been "open the app and find out". This gives it an answer at a
//! glance, and — the reason it exists — it is still running when the hub is taken away.
//!
//! Every judgement about what to show and when to interrupt lives in `brvg_hub::tray_state`, which
//! is pure and tested on every platform. This file is the shell: poll, render, notify, and drive
//! start/stop. Keep it that way — the Windows half only compiles in CI, so logic that lives here
//! is logic nobody can test.

// Windows-only by nature: it draws in the Windows notification area and drives schtasks. The stub
// keeps `cargo build` honest on Linux and macOS (CI builds the daemon on Linux) instead of making
// the whole crate Windows-only.
#[cfg(not(windows))]
fn main() {
    eprintln!("brvg-hub-tray is Windows-only; the hub daemon itself is not.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    windows_impl::run();
}

#[cfg(windows)]
mod windows_impl {
    use brvg_hub::tray_state::{alert_text, for_menu, Alert, Icon, Monitor, Observation};
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    /// Matches hub_config.rs's DEFAULT_HTTP_PORT. The hub can be moved off it, but a tray app that
    /// asked the user for a port number would be a worse product than one that occasionally says
    /// "not answering" on an unusual setup.
    const PORT: u16 = 8722;
    const TASK_NAME: &str = "BoatRVGuardianHub";
    /// 15s: frequent enough that a removal is noticed while the user is still near the machine,
    /// cheap enough to be free — it is a loopback request to a process on the same box.
    const POLL: Duration = Duration::from_secs(15);
    /// Keeps every helper process from flashing a console window. Without it the user gets a black
    /// rectangle on screen every 15 seconds, which is its own kind of broken.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn hub_dir() -> PathBuf {
        PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into()))
            .join("BoatRVGuardian")
    }

    /// One poll. Deliberately returns FACTS, not conclusions — the interpretation belongs to
    /// `tray_state`, where it is tested.
    fn look() -> Observation {
        let (answering, registered) = ping();
        Observation {
            answering,
            registered,
            binary_present: hub_dir().join("bin").join("brvg-hub.exe").exists(),
            task_present: task_registered(),
        }
    }

    fn ping() -> (bool, bool) {
        let url = format!("http://127.0.0.1:{PORT}/api/hub/ping");
        // Blocking client built per call: this runs every 15s on a background thread, so the cost
        // is irrelevant, and a long-lived client holding a socket to a process we are watching die
        // is a worse trade.
        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        else {
            return (false, false);
        };
        match client.get(url).send() {
            Ok(r) if r.status().is_success() => {
                let registered = r
                    .json::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v.get("registered").and_then(|b| b.as_bool()))
                    .unwrap_or(false);
                (true, registered)
            }
            _ => (false, false),
        }
    }

    fn task_registered() -> bool {
        Command::new("schtasks")
            .args(["/Query", "/TN", TASK_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Start/stop need admin because the task runs as SYSTEM. Owner ruling 2026-08-20: acceptable,
    /// because installing the hub already prompts. One UAC per action, never a standing elevation.
    fn run_elevated(inner: &str) {
        let script = format!("Start-Process -FilePath schtasks -ArgumentList '{inner}' -Verb RunAs -WindowStyle Hidden");
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }

    /// A flat RGBA square. Deliberately not an .ico asset: four solid colours read correctly at
    /// 16px, need no designer, and cannot go missing from the installer's file list.
    fn icon_for(state: Icon) -> tray_icon::Icon {
        const N: u32 = 32;
        let (r, g, b) = match state {
            Icon::Ok => (0x22, 0xA5, 0x5A),           // green — watching
            Icon::NeedsSigning => (0xE0, 0x9B, 0x20), // amber — running, not signed to a vehicle
            Icon::Bad => (0xC8, 0x32, 0x32),          // red — should be running and is not
            Icon::Absent => (0x8A, 0x8A, 0x8A),       // grey — no hub here
        };
        let mut rgba = Vec::with_capacity((N * N * 4) as usize);
        for _ in 0..N * N {
            rgba.extend_from_slice(&[r, g, b, 0xFF]);
        }
        tray_icon::Icon::from_rgba(rgba, N, N).expect("tray icon is a fixed-size RGBA buffer")
    }

    fn tooltip(state: Icon) -> &'static str {
        match state {
            Icon::Ok => "Boat & RV Guardian hub — watching this vehicle",
            Icon::NeedsSigning => "Boat & RV Guardian hub — running, but not signed to a vehicle",
            Icon::Bad => "Boat & RV Guardian hub — NOT running. This vehicle is not being watched.",
            Icon::Absent => "Boat & RV Guardian hub — not installed on this computer",
        }
    }

    fn notify(alert: Alert) {
        let (title, body) = alert_text(alert);
        let _ = notify_rust::Notification::new()
            .summary(title)
            .body(body)
            .show();
    }

    pub fn run() {
        let event_loop = EventLoopBuilder::new().build();

        let menu = Menu::new();
        let m_status = MenuItem::new("Checking the hub…", false, None);
        let m_start = MenuItem::new("Start hub", true, None);
        let m_stop = MenuItem::new("Stop hub", true, None);
        let m_quit = MenuItem::new("Quit", true, None);
        let _ = menu.append_items(&[
            &m_status,
            &PredefinedMenuItem::separator(),
            &m_start,
            &m_stop,
            &PredefinedMenuItem::separator(),
            &m_quit,
        ]);

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip(Icon::Absent))
            .with_icon(icon_for(Icon::Absent))
            .build();
        let Ok(tray) = tray else {
            eprintln!("brvg-hub-tray: could not create the tray icon");
            std::process::exit(1);
        };

        // Polling runs off the UI thread. A five-second HTTP timeout on the event loop would freeze
        // the menu for five seconds every time the hub is down — exactly when the user is trying to
        // click Start.
        let (tx, rx) = mpsc::channel::<Observation>();
        std::thread::spawn(move || loop {
            if tx.send(look()).is_err() {
                return; // the UI is gone; so are we
            }
            std::thread::sleep(POLL);
        });

        let mut monitor = Monitor::new();
        let menu_rx = MenuEvent::receiver();
        let tray_rx = TrayIconEvent::receiver();

        event_loop.run(move |_event, _, control_flow| {
            // Wake regularly rather than block: this loop owns both the poll channel and the menu
            // channel, and neither integrates with tao's own event sources.
            *control_flow =
                ControlFlow::WaitUntil(std::time::Instant::now() + Duration::from_millis(400));

            while let Ok(obs) = rx.try_recv() {
                let (icon, alert) = monitor.observe(&obs);
                let _ = tray.set_icon(Some(icon_for(icon)));
                let _ = tray.set_tooltip(Some(tooltip(icon)));
                m_status.set_text(for_menu(tooltip(icon)));
                m_start.set_enabled(icon != Icon::Ok && icon != Icon::NeedsSigning);
                m_stop.set_enabled(icon == Icon::Ok || icon == Icon::NeedsSigning);

                if let Some(a) = alert {
                    // Ask tray_state WHY it is bad rather than trusting the transition's guess: the
                    // difference between "your antivirus removed it" and "it stopped" is the whole
                    // value of the notification, and only the observation can tell them apart.
                    let precise = if matches!(a, Alert::Stopped) {
                        Monitor::diagnose(&obs)
                    } else {
                        a
                    };
                    notify(precise);
                }
            }

            if let Ok(ev) = menu_rx.try_recv() {
                if ev.id == m_quit.id() {
                    *control_flow = ControlFlow::Exit;
                } else if ev.id == m_start.id() {
                    run_elevated(&format!("/Run /TN {TASK_NAME}"));
                } else if ev.id == m_stop.id() {
                    run_elevated(&format!("/End /TN {TASK_NAME}"));
                }
            }
            while tray_rx.try_recv().is_ok() {} // drained so the queue cannot grow unbounded
        });
    }
}
