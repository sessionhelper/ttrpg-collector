//! Voice handling: audio capture, disk-backed pre-consent cache, mix producer, Discord event dispatch.

pub mod buffer;
pub mod events;
pub mod mixer;
pub mod receiver;

pub use receiver::{
    AudioHandle, AudioObservables, AudioPacket, AudioReceiver, DaveHealRequest, Op5Event,
    PacketSink,
};
