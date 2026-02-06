use bytes::{Buf, BufMut, Bytes};
use crate::enums::OMTFrameType;
use std::io;

#[derive(Debug, Clone, Default)]
pub struct OMTFrameHeader {
    pub version: u8,
    pub frame_type: OMTFrameType,
    pub timestamp: i64,
    pub metadata_length: u16,
    pub data_length: i32,
}

impl OMTFrameHeader {
    pub const SIZE: usize = 16;

    pub fn read(mut buf: impl Buf) -> Result<Self, io::Error> {
        if buf.remaining() < Self::SIZE {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Not enough bytes for header"));
        }
        let version = buf.get_u8();
        let frame_type_val = buf.get_u8();
        let timestamp = buf.get_i64_le();
        let metadata_length = buf.get_u16_le();
        let data_length = buf.get_i32_le();

        Ok(OMTFrameHeader {
            version,
            frame_type: OMTFrameType::from(frame_type_val),
            timestamp,
            metadata_length,
            data_length,
        })
    }

    pub fn write(&self, mut buf: impl BufMut) {
        buf.put_u8(self.version);
        buf.put_u8(self.frame_type as u8);
        buf.put_i64_le(self.timestamp);
        buf.put_u16_le(self.metadata_length);
        buf.put_i32_le(self.data_length);
    }
}

#[derive(Debug, Clone, Default)]
pub struct OMTVideoHeader {
    pub codec: i32,
    pub width: i32,
    pub height: i32,
    pub frame_rate_n: i32,
    pub frame_rate_d: i32,
    pub aspect_ratio: f32,
    pub flags: u32,
    pub color_space: i32,
}

impl OMTVideoHeader {
    pub const SIZE: usize = 32;

    pub fn read(mut buf: impl Buf) -> Result<Self, io::Error> {
        if buf.remaining() < Self::SIZE {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Not enough bytes for video header"));
        }
        Ok(OMTVideoHeader {
            codec: buf.get_i32_le(),
            width: buf.get_i32_le(),
            height: buf.get_i32_le(),
            frame_rate_n: buf.get_i32_le(),
            frame_rate_d: buf.get_i32_le(),
            aspect_ratio: buf.get_f32_le(),
            flags: buf.get_i32_le() as u32,
            color_space: buf.get_i32_le(),
        })
    }

    pub fn write(&self, mut buf: impl BufMut) {
        buf.put_i32_le(self.codec);
        buf.put_i32_le(self.width);
        buf.put_i32_le(self.height);
        buf.put_i32_le(self.frame_rate_n);
        buf.put_i32_le(self.frame_rate_d);
        buf.put_f32_le(self.aspect_ratio);
        buf.put_i32_le(self.flags as i32);
        buf.put_i32_le(self.color_space);
    }
}

#[derive(Debug, Clone, Default)]
pub struct OMTAudioHeader {
    pub codec: i32,
    pub sample_rate: i32,
    pub samples_per_channel: i32,
    pub channels: i32,
    pub active_channels: u32,
    pub reserved1: i32,
}

impl OMTAudioHeader {
    pub const SIZE: usize = 24;

    pub fn read(mut buf: impl Buf) -> Result<Self, io::Error> {
        if buf.remaining() < Self::SIZE {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Not enough bytes for audio header"));
        }
        Ok(OMTAudioHeader {
            codec: buf.get_i32_le(),
            sample_rate: buf.get_i32_le(),
            samples_per_channel: buf.get_i32_le(),
            channels: buf.get_i32_le(),
            active_channels: buf.get_u32_le(),
            reserved1: buf.get_i32_le(),
        })
    }

    pub fn write(&self, mut buf: impl BufMut) {
        buf.put_i32_le(self.codec);
        buf.put_i32_le(self.sample_rate);
        buf.put_i32_le(self.samples_per_channel);
        buf.put_i32_le(self.channels);
        buf.put_u32_le(self.active_channels);
        buf.put_i32_le(self.reserved1);
    }
}

// Helper struct to hold a complete frame
#[derive(Debug, Clone)]
pub struct OMTFrame {
    pub header: OMTFrameHeader,
    pub video_header: Option<OMTVideoHeader>,
    pub audio_header: Option<OMTAudioHeader>,
    pub data: Bytes, // The payload (metadata + actual data)
}

impl OMTFrame {
    pub fn new(frame_type: OMTFrameType) -> Self {
        let mut header = OMTFrameHeader::default();
        header.version = 1;
        header.frame_type = frame_type;
        let mut frame = OMTFrame {
            header,
            video_header: None,
            audio_header: None,
            data: Bytes::new(),
        };
        frame.update_data_length();
        frame
    }

    pub fn update_data_length(&mut self) {
        let mut extended_len = 0;
        if self.header.frame_type == OMTFrameType::Video {
            extended_len = OMTVideoHeader::SIZE;
        } else if self.header.frame_type == OMTFrameType::Audio {
            extended_len = OMTAudioHeader::SIZE;
        }
        self.header.data_length = (self.data.len() + extended_len) as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use crate::enums::{OMTCodec, OMTVideoFlags, OMTColorSpace};

    #[test]
    fn test_header_serialization() {
        let mut header = OMTFrameHeader::default();
        header.version = 1;
        header.frame_type = OMTFrameType::Audio;
        header.timestamp = 12345;
        header.data_length = 100;

        let mut buf = BytesMut::with_capacity(OMTFrameHeader::SIZE);
        header.write(&mut buf);
        
        let mut read_buf = buf.freeze();
        let read_header = OMTFrameHeader::read(&mut read_buf).unwrap();

        assert_eq!(header.version, read_header.version);
        assert_eq!(header.frame_type, read_header.frame_type);
        assert_eq!(header.timestamp, read_header.timestamp);
        assert_eq!(header.data_length, read_header.data_length);
    }

    #[test]
    fn test_video_header_serialization() {
        let v_header = OMTVideoHeader {
            codec: OMTCodec::VMX1 as i32,
            width: 1920,
            height: 1080,
            frame_rate_n: 60,
            frame_rate_d: 1,
            aspect_ratio: 1.77,
            flags: OMTVideoFlags::Interlaced as u32,
            color_space: OMTColorSpace::BT709 as i32,
        };

        let mut buf = BytesMut::with_capacity(OMTVideoHeader::SIZE);
        v_header.write(&mut buf);

        let mut read_buf = buf.freeze();
        let read_v_header = OMTVideoHeader::read(&mut read_buf).unwrap();

        assert_eq!(v_header.codec, read_v_header.codec);
        assert_eq!(v_header.width, read_v_header.width);
        assert_eq!(v_header.height, read_v_header.height);
        assert_eq!(v_header.flags, read_v_header.flags);
    }
}
