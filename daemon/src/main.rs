// The BRVG hub. Running it IS the intent — there is no flag to parse and no window it could ever
// open. Installed as a boot service (Windows scheduled task / macOS LaunchDaemon) by its own
// installer; runnable by hand for a bench.
fn main() {
    brvg_hub::hub_server::run_headless();
}
