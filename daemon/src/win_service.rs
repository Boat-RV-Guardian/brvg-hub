// The daemon AS A WINDOWS SERVICE (owner ruling 2026-08-20: "build it as a service no matter what,
// i don't like it as a scheduled task"). This wraps the exact same `hub_server` runtime that a
// manual `--hub` run drives — one daemon, two ways of being told to stop — and adds the one thing a
// service must have that a bench run does not: it answers the SCM's control codes.
//
// WHY A SERVICE AND NOT THE SCHEDULED TASK IT REPLACES. Sophos flagged the task as `Persist_6a`,
// which is MITRE T1053.005 — "Scheduled Task/Job" — and the detection was BEHAVIOURAL, firing on
// the `schtasks` call pattern, not on the binary (allowing the files did nothing; only "Allow
// Behavior" worked — established on CENTRAL, 2026-08-20). A service is a different technique
// (T1543.003), so it steps out from under that specific rule. It may trip a different one — that
// is a hardware test on CENTRAL, not a claim to make from here — but a long-running background
// daemon SHOULD be a service regardless, which is the owner's actual reason.
//
// This module is Windows-only (`#[cfg(windows)]` in lib.rs). The `windows-service` crate is pinned
// at 0.6 — the long-stable line every example uses — because, as with the tray deps, nobody on this
// team can compile Windows locally, so proven beats newest. It compiles only on the `daemon-windows`
// CI leg; there is no way to run it here.

use std::ffi::OsString;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

/// The SCM name. MUST match what the installer passes to `sc create` and what the app's
/// hub_service.rs uses for every `sc` verb — a mismatch means the app manages a service the SCM
/// launched under a different name, the same class of bug as the auto-start box reading one thing
/// while the toggle wrote another.
pub const SERVICE_NAME: &str = "BoatRVGuardianHub";

/// OWN_PROCESS: this binary hosts exactly one service, itself. Not SHARE_PROCESS (svchost-style),
/// which we are not, and reporting the wrong type makes the SCM's own accounting wrong.
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

define_windows_service!(ffi_service_main, service_main);

/// Entry point for a service start. Called from `main` when the process was launched by the SCM
/// (we mark that launch with `--service` in the registered binPath). Blocks until the SCM
/// disconnects. Returns the crate error so `main` can exit non-zero if the dispatcher itself
/// failed to connect — which is exactly what happens if someone runs `brvg-hub --service` by hand
/// instead of through the SCM (error 1063), and telling them so beats a silent no-op.
pub fn run() -> Result<(), windows_service::Error> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

/// The SCM calls this on its own thread. Its Vec of args is the service's *start* arguments, which
/// we do not use — all configuration is read from ProgramData by the runtime, exactly as in a
/// manual run. Any failure here has nowhere reliable to be logged (a service has no console), so
/// it surfaces the only way a dead hub ever does: the heartbeat stops and the cloud's connectivity
/// sweep alerts on the silence. That is by design, not a gap — see run_headless's own note.
fn service_main(_args: Vec<OsString>) {
    let _ = run_service();
}

fn run_service() -> Result<(), windows_service::Error> {
    // The bridge from the SCM's synchronous control callback to the daemon's async shutdown. A
    // oneshot is right because stop happens exactly once; wrapping the sender in Option lets the
    // FnMut handler satisfy the borrow checker while still consuming the sender on the single send.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mut shutdown_tx = Some(shutdown_tx);

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            // Both mean "wind down now": Stop is `sc stop` / the Services UI, Shutdown is the OS
            // going down. A hub must treat a machine shutdown as a clean stop, not a crash, or it
            // risks leaving its files mid-write — the daemon's own shutdown path is the safe one.
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(tx) = shutdown_tx.take() {
                    let _ = tx.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            // Required to answer, even though our status never goes stale between reports.
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    // Tell the SCM we are up and which controls we honour, BEFORE starting the runtime — the SCM
    // holds `sc start` open until it sees Running, and a service that binds its port first would
    // report Running late and risk a start timeout on a slow machine.
    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Hand the SCM's stop signal to the daemon as its shutdown future. Blocks here for the whole
    // life of the service; returns when the oneshot fires (or its sender is dropped, which cannot
    // happen while the handler is registered).
    crate::hub_server::run_with_shutdown(async move {
        let _ = shutdown_rx.await;
    });

    // The runtime has wound down. Report Stopped so `sc stop` completes cleanly instead of timing
    // out — the failure mode that makes a healthy daemon look hung.
    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}
