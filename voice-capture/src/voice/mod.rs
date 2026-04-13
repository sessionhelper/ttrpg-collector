//! Voice handling: audio capture, disk-backed pre-consent cache, Discord event dispatch.

pub mod buffer;
pub mod events;
pub mod receiver;

pub use receiver::{
    AudioHandle, AudioObservables, AudioPacket, AudioReceiver, Op5Event, PacketSink,
};
