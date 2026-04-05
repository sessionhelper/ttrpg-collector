//! Voice handling: audio capture (receiver) and Discord event dispatch (events).

pub mod events;
pub mod receiver;

pub use receiver::{AudioHandle, AudioPacket, AudioReceiver};
