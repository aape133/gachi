use gachi_proto::audio::{MAX_CHANNELS, MAX_FRAMES};

pub const MAX_FRAME_SAMPLES: usize = MAX_FRAMES * MAX_CHANNELS;

/// One network packet worth of audio, fixed-size so it can move through
/// lock-free rings without allocating.
#[derive(Clone, Copy)]
pub struct Frame {
    pub seq: u32,
    pub channels: u8,
    pub frames: u16,
    pub data: [f32; MAX_FRAME_SAMPLES],
}

impl Frame {
    pub const EMPTY: Frame = Frame {
        seq: 0,
        channels: 2,
        frames: 0,
        data: [0.0; MAX_FRAME_SAMPLES],
    };

    pub fn samples(&self) -> &[f32] {
        &self.data[..self.frames as usize * self.channels as usize]
    }
}

impl Default for Frame {
    fn default() -> Self {
        Self::EMPTY
    }
}
