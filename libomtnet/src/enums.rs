use serde::{Deserialize, Serialize};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OMTFrameType {
    None = 0,
    Metadata = 1,
    Video = 2,
    Audio = 4,
}

impl Default for OMTFrameType {
    fn default() -> Self {
        OMTFrameType::None
    }
}

impl From<u8> for OMTFrameType {
    fn from(value: u8) -> Self {
        match value {
            1 => OMTFrameType::Metadata,
            2 => OMTFrameType::Video,
            4 => OMTFrameType::Audio,
            _ => OMTFrameType::None,
        }
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OMTVideoFlags {
    None = 0,
    Interlaced = 1,
    Alpha = 2,
    PreMultiplied = 4,
    Preview = 8,
    HighBitDepth = 16,
}

// Helper to handle bitwise operations for flags if needed, or use bitflags! crate
// For now treating as u32 in structs often works, but let's provide safe helpers if needed.

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OMTColorSpace {
    Undefined = 0,
    BT601 = 601,
    BT709 = 709,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OMTPlatformType {
    Unknown = 0,
    Win32 = 1,
    MacOS = 2,
    Linux = 3,
    #[allow(non_camel_case_types)]
    iOS = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OMTCodec {
    VMX1 = 0x31584D56,
    FPA1 = 0x31415046, // Planar audio
    UYVY = 0x59565955,
    YUY2 = 0x32595559,
    BGRA = 0x41524742,
    NV12 = 0x3231564E,
    YV12 = 0x32315659,
    UYVA = 0x41565955,
    P216 = 0x36313250,
    PA16 = 0x36314150,
}

impl Into<i32> for OMTCodec {
    fn into(self) -> i32 {
        self as i32
    }
}
