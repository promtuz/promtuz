//! Relay library surface. The relay is primarily a binary (`main.rs`);
//! the lib target exists so the `ldb` admin bin can read the fjall
//! store through the same `MessageKey`/`Store` code.

pub mod storage;
