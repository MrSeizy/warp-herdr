//! herdr integration ("herdr control mode"): mirrors tabs and panes from an
//! external herdr orchestrator into Warp's workspace over herdr's JSON socket
//! API. The socket is a Unix domain socket (on Windows,
//! `std::os::windows::net::UnixStream`); wasm compiles against a stub whose
//! calls return errors. Exercised on Unix only. Runtime-gated by
//! `FeatureFlag::HerdrControlMode`.

pub mod connection;
#[cfg(test)]
pub mod fake_herdr;
pub mod layout_convert;
pub mod protocol;
pub mod reconciler;
pub mod socket;
