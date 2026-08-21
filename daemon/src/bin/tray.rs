//! `brvg-hub-tray` — the hub's presence in the notification area.
//!
//! The hub is a SYSTEM Windows service with no window, which means the honest answer to "is my boat
//! being watched right now?" has been "open the app and find out". This gives it an answer at a
//! glance, and — the reason it exists — it is still running when the hub is taken away.
//!
//! Every judgement about what to show and when to interrupt lives in `brvg_hub::tray_state`, which
//! is pure and tested on every platform. This file is the shell: poll, render, notify, and drive
//! start/stop. Keep it that way — the Windows half only compiles in CI, so logic that lives here
//! is logic nobody can test.

// Windows-only by nature: it draws in the Windows notification area and drives the SCM (`sc`). The
// stub keeps `cargo build` honest on Linux and macOS (CI builds the daemon on Linux) instead of
// making the whole crate Windows-only.
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
    use brvg_hub::tray_state::{
        alert_text, for_menu, paint_status_dot, status_rgb, Alert, Icon, Monitor, Observation,
    };
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
    /// The Windows service name — MUST match the daemon's `win_service.rs` SERVICE_NAME and the
    /// app's hub_service.rs SERVICE_NAME. Same string across all three; a mismatch means the tray
    /// reports on, and starts/stops, a service nobody else is managing.
    const SERVICE_NAME: &str = "BoatRVGuardianHub";
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
            service_present: service_registered(),
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

    /// `sc query <name>` succeeds iff the service exists — unelevated, allowed for authenticated
    /// users under the SCM default (same reason the app's status poll needs no elevation).
    fn service_registered() -> bool {
        Command::new("sc.exe")
            .args(["query", SERVICE_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Start/stop need admin because the service runs as LocalSystem. Owner ruling 2026-08-20:
    /// acceptable, because installing the hub already prompts. One UAC per action, never a standing
    /// elevation. `sc start`/`sc stop` are idempotent enough for a menu — a redundant click just
    /// returns a benign non-zero the tray ignores; the next 15s poll shows the true state either way.
    fn run_elevated(verb: &str) {
        let script = format!(
            "Start-Process -FilePath sc.exe -ArgumentList '{verb}','{SERVICE_NAME}' -Verb RunAs -WindowStyle Hidden"
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }

    /// The BRVG brand mark, embedded from the app's own 32×32 icon, with a coloured STATUS DOT in
    /// the corner (owner ruling 2026-08-20: "make the icon the BRVG shield"). The dot keeps the
    /// at-a-glance status the flat square used to carry — a tray icon on a boat has to answer
    /// "watching / not watching" without a click — while the mark makes it recognisably ours.
    const ICON_PNG: &[u8] = include_bytes!("../../assets/tray-icon.png");

    fn icon_for(state: Icon) -> tray_icon::Icon {
        // The brand mark is the intent, but the tray must never fail to show SOMETHING — and this
        // render path cannot be tested off-Windows — so a decode failure falls back to the old flat
        // status square rather than leaving the notification area blank.
        brand_icon(state).unwrap_or_else(|| flat_icon(state))
    }

    /// Decode the embedded PNG and paint the status dot. Returns `None` on anything unexpected
    /// (bad decode, or not the 8-bit RGBA we shipped), so the caller can fall back.
    fn brand_icon(state: Icon) -> Option<tray_icon::Icon> {
        let mut reader = png::Decoder::new(ICON_PNG).read_info().ok()?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).ok()?;
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return None;
        }
        let (w, h) = (info.width, info.height);
        buf.truncate((w * h * 4) as usize);
        paint_status_dot(&mut buf, w, h, state); // pure + host-tested in tray_state
        tray_icon::Icon::from_rgba(buf, w, h).ok()
    }

    /// The pre-2026-08-20 fallback: a flat status-coloured square. Four solid colours read
    /// correctly at 16px and need nothing on disk, which is exactly why it is the safety net.
    fn flat_icon(state: Icon) -> tray_icon::Icon {
        const N: u32 = 32;
        let (r, g, b) = status_rgb(state);
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
                    run_elevated("start");
                } else if ev.id == m_stop.id() {
                    run_elevated("stop");
                }
            }
            while tray_rx.try_recv().is_ok() {} // drained so the queue cannot grow unbounded
        });
    }
}
