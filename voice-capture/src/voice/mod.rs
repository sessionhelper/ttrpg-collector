//! Voice audio capture: receiver, buffering, and SSRC tracking.

pub mod receiver;

pub use receiver::{AudioHandle, AudioPacket, AudioReceiver};
