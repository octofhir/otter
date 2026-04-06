//! Parked Node.js compatibility runner shim.
//!
//! The original runner depended on the retired engine stack. It remains in the
//! repository as a compileable placeholder until a new-stack replacement lands.

#![warn(clippy::all)]

pub const PARKED_MESSAGE: &str =
    "node-compat is parked during the legacy stack retirement and has no active runtime backend";

#[must_use]
pub const fn parked_message() -> &'static str {
    PARKED_MESSAGE
}
