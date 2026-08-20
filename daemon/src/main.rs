// The BRVG hub. Running it IS the intent — there is no window it could ever open.
//
// One argument matters, and only on Windows: `--service`. The installer registers the service with
// a binPath of `"...brvg-hub.exe" --service`, so that flag is present precisely when the Windows
// SCM launched us and expects us to speak its control protocol. Every other launch — a bench run,
// the app's transitional `--hub` path, macOS, Linux — is a plain foreground daemon that stops on
// ctrl-c. The flag is never inferred; a hub started by hand must never try to connect to the SCM
// (that fails with error 1063), and the SCM launch must never fall through to the foreground path
// (it would exit instantly, and the service would look like it crashed on start).
fn main() {
    #[cfg(windows)]
    {
        if std::env::args().any(|a| a == "--service") {
            if let Err(e) = brvg_hub::win_service::run() {
                eprintln!("hub: service dispatcher failed: {e}");
                std::process::exit(1);
            }
            return;
        }
    }
    brvg_hub::hub_server::run_headless();
}
