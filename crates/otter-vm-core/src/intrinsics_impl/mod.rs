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
//! - `reflect` - Reflect namespace (all 13 ES2015+ methods)
//! - `map_set` - Map/Set/WeakMap/WeakSet constructors and prototype methods (ES2026)
//! - `regexp` - RegExp constructor and prototype methods (ES2026)
//! - `promise` - Promise constructor statics and prototype methods (ES2026)
//! - `generator` - Generator.prototype and AsyncGenerator.prototype methods (ES2026)
//! - `typed_array` - %TypedArray%.prototype and all 11 typed array prototypes (ES2026)
//! - `proxy` - Proxy constructor and static methods (ES2026)
//! - `object` - Object.prototype methods and Object static methods (ES2026)
//! - `error` - Error.prototype and all error type prototypes with stack trace support (ES2026)

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
pub mod map_set;
pub mod regexp;
pub mod promise;
pub mod generator;
pub mod typed_array;
pub mod proxy;
pub mod object;
pub mod error;
