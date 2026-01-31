//! Intrinsics implementation modules
//!
//! This module contains helper functions and separate implementations
//! for each builtin prototype (String, Number, Date, Array, etc.)
//!
//! ## Current modules:
//! - `helpers` - Utility functions (strict_equal, same_value_zero, array helpers)
//! - `date` - Date.prototype methods (all ES2026 methods)
//! - `string` - String.prototype methods (all ES2026 methods)
//! - `number` - Number.prototype methods (all ES2026 methods)
//! - `array` - Array.prototype methods (all ES2026 methods)
//! - `function` - Function.prototype methods (call, apply, bind, toString)
//! - `boolean` - Boolean constructor and prototype methods
//! - `temporal` - Temporal namespace initialization (Instant, PlainDate, etc.)
//! - `math` - Math namespace initialization (constants and methods)

pub mod helpers;
pub mod date;
pub mod string;
pub mod number;
pub mod array;
pub mod function;
pub mod boolean;
pub mod temporal;
pub mod math;
pub mod reflect;
