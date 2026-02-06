use bytes::{Buf, BytesMut};
use tokio_util::codec::{Decoder, Encoder};
use crate::frame::{OMTFrame, OMTFrameHeader, OMTVideoHeader, OMTAudioHeader};
use crate::enums::OMTFrameType;
use std::io;

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
            },
            OMTFrameType::Audio => {
                let ah = OMTAudioHeader::read(&mut *src)?;
                frame.audio_header = Some(ah);
            },
            _ => {}
        }

        // Read Data
        // Data len is header.data_length - extended_header_size
        let payload_len = header.data_length as usize - extended_header_size;
        let metadata_len = header.metadata_length as usize;

        if payload_len > 0 {
            let meta_len = metadata_len.min(payload_len);
            if meta_len > 0 {
                frame.metadata = src.split_to(meta_len).freeze();
            }
            let remaining = payload_len - meta_len;
            if remaining > 0 {
                frame.data = src.split_to(remaining).freeze();
            }
        }

        Ok(Some(frame))
    }
}

impl Encoder<OMTFrame> for OMTFrameCodec {
    type Error = io::Error;

    fn encode(&mut self, item: OMTFrame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        // 1. Write Header
        item.header.write(&mut *dst);

        // 2. Write Extended Header
        if let Some(ref vh) = item.video_header {
            if item.header.frame_type == OMTFrameType::Video {
                vh.write(&mut *dst);
            }
        } else if let Some(ref ah) = item.audio_header {
            if item.header.frame_type == OMTFrameType::Audio {
                ah.write(&mut *dst);
            }
        }

        // 3. Write Metadata + Data
        if !item.metadata.is_empty() {
            dst.extend_from_slice(&item.metadata);
        }
        if !item.data.is_empty() {
            dst.extend_from_slice(&item.data);
        }
        
        Ok(())
    }
}
