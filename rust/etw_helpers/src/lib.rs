// `no_std` (with `alloc`) unless the `std` feature is enabled, which every channel backend needs.
#![cfg_attr(not(any(feature = "std", test)), no_std)]

extern crate alloc;

pub mod consumer;
pub mod controller;
mod error;
mod profiling;

pub use error::{Error, hresult_from_win32};
#[cfg(test)]
pub(crate) mod test_util {
    use windows_sys::core::GUID;

    // Absolute path of the `.etl` file `name` in the target directory: `CARGO_TARGET_DIR`,
    // or the package's `target` directory by default.
    pub(crate) fn etl_path(name: &str) -> String {
        let target_dir = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
        std::path::absolute(target_dir.join(name))
            .expect("invalid target directory")
            .to_string_lossy()
            .into_owned()
    }

    // windows-sys GUIDs don't implement PartialEq.
    pub(crate) fn guid_eq(a: &GUID, b: &GUID) -> bool {
        a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
    }

    // Waits until a session has enabled `provider`.
    pub(crate) fn wait_until_enabled(provider: &tracelogging_dynamic::Provider) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !provider.enabled(tracelogging::Level::LogAlways, 0) {
            assert!(
                std::time::Instant::now() < deadline,
                "provider was never enabled"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
