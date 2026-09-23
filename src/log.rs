use tracing_subscriber::EnvFilter;

pub fn configure_logger() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("kvs=info")),
        )
        .with_writer(std::io::stderr)
        .with_thread_names(true)
        .with_thread_ids(true)
        .init();
}
