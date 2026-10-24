#[cfg(not(feature = "rust-backend"))]
compile_error!(
    "mars-xlog requires the `rust-backend` feature; the C++ backend is repository-local only"
);

mod metrics;
mod rust;

pub(crate) use rust::RustBackend;

pub(crate) fn provider() -> &'static rust::RustBackendProvider {
    rust::provider()
}
