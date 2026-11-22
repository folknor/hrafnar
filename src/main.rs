/// The keeper is dispatched before the tokio runtime exists. It is a
/// synchronous, single-threaded process that owns every wait status in its
/// tree (see `hrafnar::keeper`), so it must never share a process with tokio's
/// child reaper or with `hrafnar`'s signal supervisor.
fn main() -> std::process::ExitCode {
    if std::env::args_os().nth(1).as_deref() == Some(hrafnar::keeper::ARG.as_ref()) {
        std::process::exit(hrafnar::keeper::main());
    }
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(hrafnar::app::run()));
    match result {
        Ok(exit) => exit.into(),
        // The form `fn main() -> anyhow::Result<()>` printed, kept so an
        // error reads as it always has.
        Err(e) => {
            eprintln!("Error: {e:?}");
            hrafnar::exit::CommandExit::Failed.into()
        }
    }
}
