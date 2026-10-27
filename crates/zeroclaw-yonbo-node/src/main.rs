#[cfg(any(target_os = "linux", target_os = "macos"))]
mod config;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod crypto;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod runtime;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod store;
#[cfg(any(target_os = "linux", target_os = "macos"))]
type Result<T> = std::result::Result<T, &'static str>;

fn main() -> std::process::ExitCode {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        runtime::main()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        eprintln!("ERR unsupported_platform");
        std::process::ExitCode::from(78)
    }
}
