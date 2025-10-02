use cpal::{
    ChannelCount, FrameCount, SampleFormat, SampleRate, SupportedBufferSize, SupportedStreamConfig,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(remote = "SampleRate")]
pub struct SampleRateDef(pub u32);

#[derive(Serialize, Deserialize)]
#[serde(remote = "SupportedBufferSize")]
pub enum SupportedBufferSizeDef {
    Range { min: FrameCount, max: FrameCount },
    Unknown,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "SampleFormat")]
#[non_exhaustive]
pub enum SampleFormatDef {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "SupportedStreamConfig")]
pub struct SupportedStreamConfigDef {
    #[serde(getter = "SupportedStreamConfig::channels")]
    channels: ChannelCount,
    #[serde(getter = "SupportedStreamConfig::sample_rate", with = "SampleRateDef")]
    sample_rate: SampleRate,
    #[serde(
        getter = "SupportedStreamConfig::buffer_size",
        with = "SupportedBufferSizeDef"
    )]
    buffer_size: SupportedBufferSize,
    #[serde(
        getter = "SupportedStreamConfig::sample_format",
        with = "SampleFormatDef"
    )]
    sample_format: SampleFormat,
}

impl From<SupportedStreamConfigDef> for SupportedStreamConfig {
    fn from(def: SupportedStreamConfigDef) -> SupportedStreamConfig {
        SupportedStreamConfig::new(
            def.channels,
            def.sample_rate,
            def.buffer_size,
            def.sample_format,
        )
    }
}

#[derive(Serialize)]
pub struct SupportedStreamConfigSerialize<'a>(
    #[serde(with = "SupportedStreamConfigDef")] pub &'a SupportedStreamConfig,
);

#[derive(Deserialize)]
pub struct SupportedStreamConfigDeserialize(
    #[serde(with = "SupportedStreamConfigDef")] pub SupportedStreamConfig,
);

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OABSMessage {
    ClientConnected {},
    AuthenticateRequest { salt: String, verifier: Vec<u8> },
    Authenticate { verifier: Vec<u8> },
    ClientId { id: String },
    ClientDisconnected {},
}
