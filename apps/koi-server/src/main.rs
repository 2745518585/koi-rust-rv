fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!(app = koi_core::APP_NAME, "koi-rust-rv scaffold is ready");
    let _ = (koi_api::CRATE_NAME, koi_infra::CRATE_NAME);
}
