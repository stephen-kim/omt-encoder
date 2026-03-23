pub const NETWORK_SEND_BUFFER: usize = 65536;
pub const NETWORK_SEND_RECEIVE_BUFFER: usize = 65536;
pub const NETWORK_RECEIVE_BUFFER: usize = 1048576 * 8; // 8MB

pub const NETWORK_ASYNC_COUNT: usize = 4;
pub const NETWORK_ASYNC_BUFFER_AV: usize = 1048576;
pub const NETWORK_ASYNC_BUFFER_META: usize = 65536;

pub const VIDEO_MIN_SIZE: usize = 65536;
pub const VIDEO_MAX_SIZE: usize = 10485760;

pub const AUDIO_MIN_SIZE: usize = 65536;
pub const AUDIO_MAX_SIZE: usize = 1048576;

pub const NETWORK_PORT_START: u16 = 6400;
pub const NETWORK_PORT_END: u16 = 6600;

pub const AUDIO_SAMPLE_SIZE: usize = 4;
pub const METADATA_MAX_COUNT: usize = 60;

pub const METADATA_FRAME_SIZE: usize = 65536;

pub const URL_PREFIX: &str = "omt://";
