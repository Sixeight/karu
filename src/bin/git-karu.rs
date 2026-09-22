// Same program under the name git looks for, so `git karu` works.
fn main() -> anyhow::Result<std::process::ExitCode> {
    karu::cli::run()
}
