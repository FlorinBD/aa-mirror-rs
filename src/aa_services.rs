//! This crate provides service implementation for  [Android Open Accessory Protocol 1.0](https://source.android.com/devices/accessories/aoa)
use log::log_enabled;
use simplelog::*;
use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use nusb::DeviceInfo;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::timeout;
use tokio_uring::buf::BoundedBuf;

// protobuf stuff:
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use crate::aa_services::protos::navigation_maneuver::NavigationType::*;
use crate::aa_services::protos::auth_response::Status::*;
use crate::aa_services::protos::Config as ChConfig;
use crate::aa_services::protos::config::Status::*;
use crate::aa_services::sensor_source_service::Sensor;
use crate::aa_services::AudioStreamType::*;
use crate::aa_services::ByeByeReason::USER_SELECTION;
use crate::aa_services::MessageStatus::*;
use crate::aa_services::MediaMessageId::*;
use crate::aa_services::InputMessageId::*;
use crate::aa_services::GalVerificationVendorExtensionMessageId::*;
use crate::aa_services::SensorMessageId::*;
use crate::aa_services::SensorType::*;
use crate::aa_services::MediaCodecType::*;
use protobuf::text_format::print_to_string_pretty;
use protobuf::{Enum, EnumOrUnknown, Message, MessageDyn};
use protos::*;
use protos::ControlMessageType::{self, *};
use crate::aoa::AccessoryDeviceInfo;
use crate::channel_manager::{Packet, ENCRYPTED, FRAME_TYPE_FIRST, FRAME_TYPE_LAST};
use crate::io_uring::Endpoint;
use crate::io_uring::IoDevice;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
#[derive(Copy, Clone, Debug)]
pub enum ServiceType
{
    InputSource,
    MediaSink,
    MediaSource,
    SensorSource,
    VendorExtension,
}

pub enum VideoCodecResolution {
    Video_800x480 = 1,
    Video_1280x720 = 2,
    Video_1920x1080 = 3,
    Video_2560x1440 = 4,
    Video_3840x2160 = 5,
    Video_720x1280 = 6,
    Video_1080x1920 = 7,
    Video_1440x2560 = 8,
    Video_2160x3840 = 9,
}

pub enum VideoFPS {
    FPS_60 = 1,
    FPS_30 = 2,
}

pub enum MediaCodec {
    AUDIO_PCM = 1,
    AUDIO_AAC_LC = 2,
    VIDEO_H264_BP = 3,
    AUDIO_AAC_LC_ADTS = 4,
    VIDEO_VP9 = 5,
    VIDEO_AV1 = 6,
    VIDEO_H265 = 7,
}
pub struct VideoConfig
{
    pub resolution : VideoCodecResolution,
    pub fps: VideoFPS,
    pub codec: MediaCodec,
}
impl fmt::Display for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
        // or, alternatively:
        // fmt::Debug::fmt(self, f)
    }
}
pub async fn th_sensor_source(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()> {
    info!( "{}: Starting...", get_name());
    let mut sdreq = ChannelOpenRequest::new();
    sdreq.set_priority(0);
    sdreq.set_service_id(ch_id);
    let mut payload: Vec<u8> = sdreq.write_to_bytes().expect("serialization failed");
    payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
    payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: ch_id as u8,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    tx_srv.send(pkt_rsp).await.expect("TODO: panic message");

    let pkt = rx_srv.recv().await.ok_or("service reader channel hung up")?;
    if pkt.channel != ch_id as u8
    {
        error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
    } else { //Channel messages
        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        if message_id != MESSAGE_CHANNEL_OPEN_RESPONSE as i32
        {
            error!( "{}, channel {:?}: Wrong message received: {}", get_name(), pkt.channel, message_id);
        }
        else {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                }
            }
            else {
                error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
            }

        }
    }
    loop {
        let pkt = rx_srv.recv().await.ok_or("service reader channel hung up")?;
        if pkt.channel != ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
        } else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
            }
            else {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }
    }
    fn get_name() -> String {
        let dev = "SensorSourceService";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_media_sink_video(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>, vcfg:VideoConfig)-> Result<()>{
    info!( "{}: Starting...", get_name());
    /* -----------------------------------OPEN CHANNEL----------------------------------------------- */
    let mut sdreq= ChannelOpenRequest::new();
    sdreq.set_priority(0);
    sdreq.set_service_id(ch_id);
    let mut payload: Vec<u8>=sdreq.write_to_bytes().expect("serialization failed");
    payload.insert(0,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
    payload.insert( 1,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: ch_id as u8,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
    let pkt = rx_srv.recv().await.ok_or("service reader channel hung up")?;
    if pkt.channel != ch_id as u8
    {
        error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
    } else { //Channel messages
        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        if message_id != MESSAGE_CHANNEL_OPEN_RESPONSE as i32
        {
            error!( "{}, channel {:?}: Wrong message received: {}", get_name(), pkt.channel, message_id);
        }
        else {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                }
            }
            else {
                error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
            }

        }
    }
    /* -----------------------------------CHANNEL CONFIG----------------------------------------------- */
    let mut cfg_req= Setup::new();
    cfg_req.set_type(MEDIA_CODEC_VIDEO_H264_BP);

    let mut payload: Vec<u8>=cfg_req.write_to_bytes().expect("serialization failed");
    payload.insert(0,((MEDIA_MESSAGE_SETUP as u16) >> 8) as u8);
    payload.insert( 1,((MEDIA_MESSAGE_SETUP as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: ch_id as u8,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    tx_srv.send(pkt_rsp).await.expect("TODO: panic message");

    loop {
        let pkt=  rx_srv.recv().await.ok_or("service reader channel hung up")?;
        if pkt.channel !=ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded",get_name(), pkt.channel);
        }
        else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            if message_id == MEDIA_MESSAGE_CONFIG  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChConfig::parse_from_bytes(&data) {
                    info!( "{}, channel {:?}: Message status: {:?}", get_name(), pkt.channel, rsp.status());
                    if rsp.status() == STATUS_READY
                    {
                        info!( "{}, channel {:?}: Starting video capture", get_name(), pkt.channel);
                    }
                }
                else {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }

    }

    fn get_name() -> String {
        let dev = "MediaSinkService Video";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_media_sink_audio_guidance(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()>{
    info!( "{}: Starting...", get_name());
    let mut sdreq= ChannelOpenRequest::new();
    sdreq.set_priority(0);
    sdreq.set_service_id(ch_id);
    let mut payload: Vec<u8>=sdreq.write_to_bytes().expect("serialization failed");
    payload.insert(0,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
    payload.insert( 1,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: ch_id as u8,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
    let pkt = rx_srv.recv().await.ok_or("service reader channel hung up")?;
    if pkt.channel != ch_id as u8
    {
        error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
    } else { //Channel messages
        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        if message_id != MESSAGE_CHANNEL_OPEN_RESPONSE as i32
        {
            error!( "{}, channel {:?}: Wrong message received: {}", get_name(), pkt.channel, message_id);
        }
        else {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                }
            }
            else {
                error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
            }

        }
    }
    loop {
        let pkt=  rx_srv.recv().await.ok_or("service reader channel hung up")?;
        if pkt.channel !=ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded",get_name(), pkt.channel);
        }
        else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
            }
            else {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }

    }

    fn get_name() -> String {
        let dev = "MediaSinkService Audio Guidance";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_media_sink_audio_streaming(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()>{
    info!( "{}: Starting...", get_name());
    let mut sdreq= ChannelOpenRequest::new();
    sdreq.set_priority(0);
    sdreq.set_service_id(ch_id);
    let mut payload: Vec<u8>=sdreq.write_to_bytes().expect("serialization failed");
    payload.insert(0,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
    payload.insert( 1,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: ch_id as u8,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
    let pkt = rx_srv.recv().await.ok_or("service reader channel hung up")?;
    if pkt.channel != ch_id as u8
    {
        error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
    } else { //Channel messages
        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        if message_id != MESSAGE_CHANNEL_OPEN_RESPONSE as i32
        {
            error!( "{}, channel {:?}: Wrong message received: {}", get_name(), pkt.channel, message_id);
        }
        else {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                }
            }
            else {
                error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
            }

        }
    }
    loop {
        let pkt=  rx_srv.recv().await.ok_or("service reader channel hung up")?;
        if pkt.channel !=ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded",get_name(), pkt.channel);
        }
        else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
            }
            else {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }

    }

    fn get_name() -> String {
        let dev = "MediaSinkService Audio Streaming";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_media_source(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()>{
    info!( "{}: Starting...", get_name());
    let mut sdreq= ChannelOpenRequest::new();
    sdreq.set_priority(0);
    sdreq.set_service_id(ch_id);
    let mut payload: Vec<u8>=sdreq.write_to_bytes().expect("serialization failed");
    payload.insert(0,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
    payload.insert( 1,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: ch_id as u8,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
    let pkt = rx_srv.recv().await.ok_or("service reader channel hung up")?;
    if pkt.channel != ch_id as u8
    {
        error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
    } else { //Channel messages
        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        if message_id != MESSAGE_CHANNEL_OPEN_RESPONSE as i32
        {
            error!( "{}, channel {:?}: Wrong message received: {}", get_name(), pkt.channel, message_id);
        }
        else {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                }
            }
            else {
                error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
            }

        }
    }
    loop {
        let pkt=  rx_srv.recv().await.ok_or("service reader channel hung up")?;
        if pkt.channel != ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
        } else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
            }
            else {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }
    }

    fn get_name() -> String {
        let dev = "MediaSourceService";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_input_source(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()>{
    info!( "{}: Starting...", get_name());
    let mut sdreq= ChannelOpenRequest::new();
    sdreq.set_priority(0);
    sdreq.set_service_id(ch_id);
    let mut payload: Vec<u8>=sdreq.write_to_bytes().expect("serialization failed");
    payload.insert(0,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
    payload.insert( 1,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: ch_id as u8,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
    let pkt = rx_srv.recv().await.ok_or("service reader channel hung up")?;
    if pkt.channel != ch_id as u8
    {
        error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
    } else { //Channel messages
        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        if message_id != MESSAGE_CHANNEL_OPEN_RESPONSE as i32
        {
            error!( "{}, channel {:?}: Wrong message received: {}", get_name(), pkt.channel, message_id);
        }
        else {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                }
            }
            else {
                error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
            }

        }
    }
    loop {
        let pkt=  rx_srv.recv().await.ok_or("service reader channel hung up")?;
        if pkt.channel != ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
        } else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
            }
            else {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }
    }

    fn get_name() -> String {
        let dev = "InputSourceService";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_vendor_extension(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()>{
    info!( "{}: Starting...", get_name());
    let mut sdreq= ChannelOpenRequest::new();
    sdreq.set_priority(0);
    sdreq.set_service_id(ch_id);
    let mut payload: Vec<u8>=sdreq.write_to_bytes().expect("serialization failed");
    payload.insert(0,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
    payload.insert( 1,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: ch_id as u8,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
    let pkt = rx_srv.recv().await.ok_or("service reader channel hung up")?;
    if pkt.channel != ch_id as u8
    {
        error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
    } else { //Channel messages
        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        if message_id != MESSAGE_CHANNEL_OPEN_RESPONSE as i32
        {
            error!( "{}, channel {:?}: Wrong message received: {}", get_name(), pkt.channel, message_id);
        }
        else {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                }
            }
            else {
                error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
            }

        }
    }
    loop {
        let pkt=  rx_srv.recv().await.ok_or("service reader channel hung up")?;
        if pkt.channel != ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
        } else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
            }
            else {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }
    }

    fn get_name() -> String {
        let dev = "VendorExtensionService";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}