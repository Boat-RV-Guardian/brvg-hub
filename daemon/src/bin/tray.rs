//! `brvg-hub-tray` — the hub's presence in the notification area (Windows) and the menu bar (macOS).
//!
//! The hub is a SYSTEM service with no window, which means the honest answer to "is my boat being
//! watched right now?" has been "open the app and find out". This gives it an answer at a glance,
//! and — the reason it exists — it is still running when the hub is taken away.
//!
//! Every judgement about what to show and when to interrupt lives in `brvg_hub::tray_state`, which
//! is pure and tested on every platform. This file is the shell: poll, render, notify, and drive
//! start/stop. Keep it that way — the Windows half only compiles in CI, so logic that lives here is
//! logic nobody can test; the macOS half at least builds and runs on a developer's own machine.
//!
//! Cross-platform since 2026-08-20 (owner: "add a taskbar icon line on windows to mac"). The event
//! loop, the icon and the poll are shared; only three things differ by platform and they live in
//! `plat`: where the daemon binary is, how to tell the service is installed, and how to start/stop
//! it (an elevated `sc` on Windows, an `osascript` admin `launchctl` on macOS).

// GUI SUBSYSTEM ON WINDOWS. Without this the tray links as a CONSOLE binary, so Windows opens a
// terminal window every time it launches — at every sign-in, since the installer starts it from the
// Run key. A notification-area monitor must be silent; this marks the PE as a GUI app so no console
// is ever created. Ignored on macOS/Linux (it only affects the Windows PE). The trade: the handful
// of `eprintln!` diagnostics below have nowhere to go on Windows — acceptable, because the user's
// signal is the tray icon itself, and never popping a window is worth losing a few dev prints.
#![windows_subsystem = "windows"]

// Linux is neither: the daemon runs there (containers) but has no desktop tray. A stub keeps
// `cargo build` honest on the Linux CI leg without making the whole crate desktop-only.
#[cfg(not(any(windows, target_os = "macos")))]
fn main() {
    eprintln!("brvg-hub-tray is for the Windows notification area and the macOS menu bar; the hub daemon itself runs anywhere.");
    std::process::exit(1);
}

#[cfg(any(windows, target_os = "macos"))]
fn main() {
    tray_impl::run();
}

/// The three things that genuinely differ between Windows and macOS. Everything else is shared.
#[cfg(any(windows, target_os = "macos"))]
mod plat {
    use std::path::PathBuf;

    #[cfg(windows)]
    mod inner {
        use std::os::windows::process::CommandExt;
        use std::path::PathBuf;
        use std::process::Command;

        /// The Windows service name — MUST match the daemon's `win_service.rs` SERVICE_NAME and the
        /// app's `hub_service.rs` SERVICE_NAME. A mismatch means the tray reports on, and
        /// starts/stops, a service nobody else is managing.
        const SERVICE_NAME: &str = "DockNeighborHub";
        /// Keeps every helper process from flashing a console window every 15 seconds.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        pub fn hub_binary_path() -> PathBuf {
            PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into()))
                .join("DockNeighbor")
                .join("bin")
                .join("brvg-hub.exe")
        }

        /// `sc query <name>` succeeds iff the service exists — unelevated, allowed for authenticated
        /// users under the SCM default (the same reason the app's status poll needs no elevation).
        pub fn service_present() -> bool {
            Command::new("sc.exe")
                .args(["query", SERVICE_NAME])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        pub fn start_hub() {
            run_elevated("start");
        }
        pub fn stop_hub() {
            run_elevated("stop");
        }

        /// Start/stop need admin because the service runs as LocalSystem. One UAC per action, never
        /// a standing elevation. A redundant click returns a benign non-zero the tray ignores; the
        /// next poll shows the true state either way.
        fn run_elevated(verb: &str) {
            let script = format!(
                "Start-Process -FilePath sc.exe -ArgumentList '{verb}','{SERVICE_NAME}' -Verb RunAs -WindowStyle Hidden"
            );
            let _ = Command::new("powershell")
                .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
        }
    }

    #[cfg(target_os = "macos")]
    mod inner {
        use std::path::PathBuf;
        use std::process::Command;

        /// The LaunchDaemon label — MUST match the daemon pkg's plist and the app's `MACOS_LABEL`.
        const LABEL: &str = "com.sc4tech.brvg-hub";
        const PLIST: &str = "/Library/LaunchDaemons/com.sc4tech.brvg-hub.plist";

        pub fn hub_binary_path() -> PathBuf {
            PathBuf::from("/Library/Application Support/DockNeighbor/bin/brvg-hub")
        }

        /// The plist is world-readable, so its presence answers "is the hub installed" with no
        /// privileges — the same signal the app's macOS status branch uses for `installed`.
        pub fn service_present() -> bool {
            std::path::Path::new(PLIST).exists()
        }

        /// Mirrors the app's macOS start: load if needed, then kickstart (a no-op on a running
        /// service). Does not `enable` — starting must not silently flip the auto-start setting, the
        /// same rule the app follows.
        pub fn start_hub() {
            admin(&format!(
                "launchctl bootstrap system {PLIST} 2>/dev/null; launchctl kickstart system/{LABEL}"
            ));
        }
        pub fn stop_hub() {
            admin(&format!("launchctl bootout system/{LABEL}"));
        }

        /// The macOS spelling of the Windows elevated call: one `osascript` admin prompt, spawned
        /// and not awaited so a menu click never blocks the UI. Starting/stopping a SYSTEM
        /// LaunchDaemon needs root, exactly like `sc` needs UAC.
        fn admin(inner: &str) {
            let script = format!(
                "do shell script \"{}\" with administrator privileges",
                inner.replace('\\', "\\\\").replace('"', "\\\"")
            );
            let _ = Command::new("osascript").args(["-e", &script]).spawn();
        }
    }

    pub fn hub_binary_path() -> PathBuf {
        inner::hub_binary_path()
    }
    pub fn service_present() -> bool {
        inner::service_present()
    }
    pub fn start_hub() {
        inner::start_hub()
    }
    pub fn stop_hub() {
        inner::stop_hub()
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod tray_impl {
    use brvg_hub::tray_state::{
        alert_text, for_menu, paint_status_dot, status_rgb, Alert, Icon, Monitor, Observation,
    };
    use std::sync::mpsc;
    use std::time::Duration;

    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    /// Matches hub_config.rs's DEFAULT_HTTP_PORT. The hub can be moved off it, but a tray app that
    /// asked the user for a port number would be a worse product than one that occasionally says
    /// "not answering" on an unusual setup.
    const PORT: u16 = 8722;
    /// 15s: frequent enough that a removal is noticed while the user is still near the machine,
    /// cheap enough to be free — it is a loopback request to a process on the same box.
    const POLL: Duration = Duration::from_secs(15);

    /// One poll. Deliberately returns FACTS, not conclusions — the interpretation belongs to
    /// `tray_state`, where it is tested.
    fn look() -> Observation {
        let (answering, registered) = ping();
        Observation {
            answering,
            registered,
            binary_present: super::plat::hub_binary_path().exists(),
            service_present: super::plat::service_present(),
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

    /// The BRVG brand mark, embedded from the app's own 32×32 icon, with a coloured STATUS DOT in
    /// the corner (owner ruling 2026-08-20: "make the icon the BRVG shield"). The dot keeps the
    /// at-a-glance status a boat tray needs — "watching / not watching" without a click — while the
    /// mark makes it recognisably ours.
    const ICON_PNG: &[u8] = include_bytes!("../../assets/tray-icon.png");

    fn icon_for(state: Icon) -> tray_icon::Icon {
        // The brand mark is the intent, but the tray must never fail to show SOMETHING — and the
        // Windows render path cannot be tested off-Windows — so a decode failure falls back to the
        // flat status square rather than leaving the notification area blank.
        brand_icon(state).unwrap_or_else(|| flat_icon(state))
    }

    /// Decode the embedded PNG and paint the status dot. Returns `None` on anything unexpected (bad
    /// decode, or not the 8-bit RGBA we shipped), so the caller can fall back.
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

    /// The fallback: a flat status-coloured square. Four solid colours read correctly at 16px and
    /// need nothing on disk, which is exactly why it is the safety net.
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
            Icon::Ok => "DockNeighbor hub — watching this vehicle",
            Icon::NeedsSigning => "DockNeighbor hub — running, but not signed to a vehicle",
            Icon::Bad => "DockNeighbor hub — NOT running. This vehicle is not being watched.",
            Icon::Absent => "DockNeighbor hub — not installed on this computer",
        }
    }

    fn notify(alert: Alert) {
        let (title, body) = alert_text(alert);
        let _ = notify_rust::Notification::new()
            .summary(title)
            .body(body)
            .show();
    }

    /// Build the event loop. On macOS it runs as an ACCESSORY app — a menu-bar item with no Dock
    /// icon and no focus-stealing on launch — which is what a background monitor should be. tao
    /// exposes this as a setter on the built loop (`EventLoopExtMacOS`), not a builder method.
    fn build_event_loop() -> tao::event_loop::EventLoop<()> {
        #[allow(unused_mut)]
        let mut event_loop = EventLoopBuilder::new().build();
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
            event_loop.set_activation_policy(ActivationPolicy::Accessory);
        }
        event_loop
    }

    pub fn run() {
        let event_loop = build_event_loop();

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
        let mut last_icon: Option<Icon> = None;
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

                // One stderr line per state CHANGE — a background app's only breadcrumb for support
                // ("what did the tray actually see"), and quiet otherwise so the log is not a firehose.
                if last_icon != Some(icon) {
                    eprintln!("brvg-hub-tray: {icon:?}");
                    last_icon = Some(icon);
                }

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
                    super::plat::start_hub();
                } else if ev.id == m_stop.id() {
                    super::plat::stop_hub();
                }
            }
            while tray_rx.try_recv().is_ok() {} // drained so the queue cannot grow unbounded
        });
    }
}
