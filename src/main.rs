fn main() -> anyhow::Result<()> {
    env_logger::init();
    let (connection, io_threads) = lsp_server::Connection::stdio();
    pbls::run(connection)?;
    io_threads.join()?;
    Ok(())
}
