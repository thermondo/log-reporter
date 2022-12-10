#[must_use]
pub(crate) fn initialize_tracing() -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish())
}
