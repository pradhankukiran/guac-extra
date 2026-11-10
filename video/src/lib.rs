// SPDX-License-Identifier: Apache-2.0
//! guac-extra video library.
//!
//! Turns a display's changed pixels into H.264 stripes for guacd, using the
//! pipeline from voxl (https://github.com/tembo/voxl, MIT): libyuv color
//! conversion, x264 encoding, stripes, paint-over and ack-based bitrate
//! control. The C API is in [`capi`]; the Rust API is [`Session`].

pub mod capi;
mod convert;
mod encoder;
mod flow;
mod governor;
pub mod log;
mod quality;
mod refine;
mod session;
mod stripes;

pub use session::{Config, Frame, Part, Rect, Session};
