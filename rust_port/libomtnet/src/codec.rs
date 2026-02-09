use crate::enums::OMTFrameType;
use crate::enums::OMTVideoFlags;
use crate::frame::{OMTAudioHeader, OMTFrame, OMTFrameHeader, OMTVideoHeader};
use bytes::{Buf, BytesMut};
use std::io;
use tokio_util::codec::{Decoder, Encoder};

pub struct OMTFrameCodec;

impl Decoder for OMTFrameCodec {
    type Item = OMTFrame;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // 1. Read Header
        if src.len() < OMTFrameHeader::SIZE {
            return Ok(None);
        }

        // Peek header first to check if we have enough data for the full frame
        // We need to construct a cursor to read without consuming if we fail later checks
        // But OMTFrameHeader::read consumes.
        // Actually, we can just peek the data length from the header bytes directly or read it.
        // Let's use a cursor to peek.
        let mut cursor = io::Cursor::new(&src[..]);
        // We know we have at least SIZE bytes
        let header = OMTFrameHeader::read(&mut cursor)?;

        // Total size on wire = Header (16) + Header.DataLength.
        // Because Header.DataLength includes ExtendedHeader + Payload.

        let total_size = OMTFrameHeader::SIZE + header.data_length as usize;

        if src.len() < total_size {
            // Not enough data yet
            // Reserve space if needed
            src.reserve(total_size - src.len());
            return Ok(None);
        }

        // We have enough data! Consume it.
        // Advance past header
        src.advance(OMTFrameHeader::SIZE);

        let mut frame = OMTFrame {
            header: header.clone(),
            video_header: None,
            audio_header: None,
            metadata: bytes::Bytes::new(),
            data: bytes::Bytes::new(),
            preview_mode: false,
            preview_data_length: None,
        };

        // Read Extended Header
        let extended_header_size = match header.frame_type {
            OMTFrameType::Video => OMTVideoHeader::SIZE,
            OMTFrameType::Audio => OMTAudioHeader::SIZE,
            _ => 0,
        };

        match header.frame_type {
            OMTFrameType::Video => {
                let vh = OMTVideoHeader::read(&mut *src)?;
                frame.video_header = Some(vh);
            }
            OMTFrameType::Audio => {
                let ah = OMTAudioHeader::read(&mut *src)?;
                frame.audio_header = Some(ah);
            }
            _ => {}
        }

        // Read Data
        // Data length includes: (data payload) + (metadata at tail)
        let payload_len = header.data_length as usize - extended_header_size;
        let metadata_len = header.metadata_length as usize;

        if payload_len > 0 {
            let meta_len = metadata_len.min(payload_len);
            let data_len = payload_len - meta_len;
            if data_len > 0 {
                frame.data = src.split_to(data_len).freeze();
            }
            if meta_len > 0 {
                frame.metadata = src.split_to(meta_len).freeze();
            }
        }

        Ok(Some(frame))
    }
}

impl Encoder<OMTFrame> for OMTFrameCodec {
    type Error = io::Error;

    fn encode(&mut self, item: OMTFrame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut wire_header = item.header.clone();
        let mut wire_video_header = item.video_header.clone();
        let ext_len = match wire_header.frame_type {
            OMTFrameType::Video => OMTVideoHeader::SIZE as i32,
            OMTFrameType::Audio => OMTAudioHeader::SIZE as i32,
            _ => 0,
        };

        if item.preview_mode {
            let desired = item.preview_data_length.unwrap_or(wire_header.data_length);
            wire_header.data_length = desired.max(ext_len);
            if let Some(ref mut vh) = wire_video_header {
                vh.flags |= OMTVideoFlags::Preview as u32;
            }
        }

        // 1. Write Header
        wire_header.write(&mut *dst);

        // 2. Write Extended Header
        if let Some(ref vh) = wire_video_header {
            if wire_header.frame_type == OMTFrameType::Video {
                vh.write(&mut *dst);
            }
        } else if let Some(ref ah) = item.audio_header {
            if wire_header.frame_type == OMTFrameType::Audio {
                ah.write(&mut *dst);
            }
        }

        // 3. Write Data + Metadata (metadata goes last per OMT protocol)
        // Respect preview data length by truncating payload bytes when requested.
        let payload_target = (wire_header.data_length - ext_len).max(0) as usize;
        let total_payload = item.data.len() + item.metadata.len();
        let send_payload = payload_target.min(total_payload);
        let metadata_to_send = item.metadata.len().min(send_payload);
        let data_to_send = send_payload.saturating_sub(metadata_to_send);

        if data_to_send > 0 {
            dst.extend_from_slice(&item.data[..data_to_send]);
        }
        if metadata_to_send > 0 {
            dst.extend_from_slice(&item.metadata[..metadata_to_send]);
        }

        Ok(())
    }
}
