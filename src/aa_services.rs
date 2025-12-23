//! This crate provides service implementation for  [Android Open Accessory Protocol 1.0](https://source.android.com/devices/accessories/aoa)
use log::log_enabled;
use simplelog::*;
use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use nusb::DeviceInfo;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::timeout;
use tokio_uring::buf::BoundedBuf;
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};
use futures::SinkExt;

// protobuf stuff:
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use crate::aa_services::protos::navigation_maneuver::NavigationType::*;
use crate::aa_services::protos::auth_response::Status::*;
use crate::aa_services::protos::Config as ChConfig;
use crate::aa_services::protos::ConfigStatus;
use crate::aa_services::protos::ConfigStatus::*;
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
use tokio_uring::net::{TcpStream, TcpListener};
use protos::*;
use protos::ControlMessageType::{self, *};
use crate::aoa::AccessoryDeviceInfo;
use crate::channel_manager::{Packet, ENCRYPTED, FRAME_TYPE_FIRST, FRAME_TYPE_LAST};
use crate::config::{TCP_DHU_PORT, TCP_VIDEO_PORT};
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
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VideoFPS {
    FPS_60 = 1,
    FPS_30 = 2,
}
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MediaCodec {
    AUDIO_PCM = 1,
    AUDIO_AAC_LC = 2,
    VIDEO_H264_BP = 3,
    AUDIO_AAC_LC_ADTS = 4,
    VIDEO_VP9 = 5,
    VIDEO_AV1 = 6,
    VIDEO_H265 = 7,
}
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AudioStream
{
    GUIDANCE=1,
    SYSTEM_AUDIO=2,
    MEDIA=3,
    TELEPHONY=4,
}
pub struct VideoConfig
{
    pub resolution : VideoCodecResolution,
    pub fps: VideoFPS,
    pub codec: MediaCodec,
}

pub struct AudioChConfiguration {
    sampling_rate:u32 ,
    number_of_bits:u32 ,
    number_of_channels:u32,
}
pub struct AudioConfig
{
    pub codec: MediaCodec,
    pub stream_type: AudioStream,
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
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = CustomCommandMessage::parse_from_bytes(&data) {
                    if msg.cmd() == CustomCommand::CMD_OPEN_CH
                    {
                        let mut open_req = ChannelOpenRequest::new();
                        open_req.set_priority(0);
                        open_req.set_service_id(ch_id);
                        let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                        payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: ch_id as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                    }
                }
                else {
                    error!( "{} CustomCommandMessage couldn't be parsed",get_name());
                }
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
    let mut video_stream_started:bool=false;
    loop {
        let pkt=  rx_srv.recv().await.ok_or("service reader channel hung up")?;
        if pkt.channel !=ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded",get_name(), pkt.channel);
        }
        else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            if message_id == ControlMessageType::MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                    if rsp.status() != MessageStatus::STATUS_SUCCESS
                    {
                        error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                    }
                    else {
                        let mut cfg_req= Setup::new();
                        cfg_req.set_type(MediaCodecType::MEDIA_CODEC_VIDEO_H264_BP);

                        let mut payload: Vec<u8>=cfg_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) >> 8) as u8);
                        payload.insert( 1,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: ch_id as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                    }
                }
                else {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == ControlMessageType::MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = CustomCommandMessage::parse_from_bytes(&data) {
                    if msg.cmd() == CustomCommand::CMD_OPEN_CH
                    {
                        let mut open_req = ChannelOpenRequest::new();
                        open_req.set_priority(0);
                        open_req.set_service_id(ch_id);
                        let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0, ((ControlMessageType::MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                        payload.insert(1, ((ControlMessageType::MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: ch_id as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                    }
                }
                else {
                    error!( "{} CustomCommandMessage couldn't be parsed",get_name());
                }
            }
            else if message_id == MediaMessageId::MEDIA_MESSAGE_CONFIG  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChConfig::parse_from_bytes(&data)
                {
                    info!( "{}, channel {:?}: Message status: {:?}", get_name(), pkt.channel, rsp.status());
                    if rsp.status() == ConfigStatus::STATUS_READY
                    {
                        info!( "{}, channel {:?}: Starting video capture", get_name(), pkt.channel);
                        if vcfg.resolution == VideoCodecResolution::Video_800x480
                        {
                            video_stream_started=true;
                            let listener_thread = tokio_uring::spawn(listen_for_connections(tx_srv.clone(),ch_id));

                            // Wait for the listener to start
                            tokio::time::sleep(Duration::from_millis(1000)).await;
                            // Prepare an FFmpeg command with separate outputs for video, audio, and subtitles.
                            FfmpegCommand::new()
                                // Global flags
                                .hide_banner()
                                // Generate test video
                                //Video Input
                                .arg("-framerate 1/30")
                                .input("/etc/aa-mirror-rs/res/130dpi.png")
                                // Video output
                                //.map("0:v")
                                .codec_video("libx264")
                                .arg("-profile:v baseline")
                                .arg("-level:v 4.0")
                                .duration("12:00:00")
                                .format("mpegts")
                                .output(format!("tcp://0.0.0.0:{TCP_VIDEO_PORT}"))
                                .print_command()
                                .spawn()?
                                .iter()?
                                .for_each(|event| match event {
                                    // Log any unexpected errors
                                    FfmpegEvent::Log(LogLevel::Warning | LogLevel::Error | LogLevel::Fatal, msg) => {
                                        error!("{msg}");
                                    }

                                    // _ => {}
                                    e => {
                                        error!("FFMpeg error: {e:?}");
                                    }
                                });
                        }
                        else
                        {
                            error!( "{}: Unsupported video resolution detected", get_name());
                        }
                    }
                }
                else
                {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else
            {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }

    }

    fn get_name() -> String {
        let dev = "MediaSinkService Video";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
    async fn listen_for_connections(mut tx: Sender<Packet>, ch_id: u8) -> Result<()> {
        let bind_addr = format!("0.0.0.0:{}", TCP_VIDEO_PORT).parse().unwrap();
        let mut listener =Some(TcpListener::bind(bind_addr).unwrap());
        //listener.set_nonblocking(true);
        let mut total_bytes_read = 0;
        loop {
            info!("Server listening on port {}", TCP_VIDEO_PORT);
            let retval =listener.as_mut().unwrap().accept();
            let (stream, addr) = match timeout(crate::io_uring::TCP_CLIENT_TIMEOUT, retval)
                .await
                .map_err(|e| std::io::Error::other(e))
            {
                Ok(Ok((stream, addr))) => (stream, addr),
                Err(e) | Ok(Err(e)) => {
                    error!("{} Video TCP server: {}, restarting...", get_name(), e);
                    continue;
                }
            };
            info!("{} Video TCP server: new client connected: <b>{:?}</b>",get_name(), addr);
            stream.set_nodelay(true).expect("TODO: panic message");
            let mut buffer:Vec<u8>;
            let mut total_bytes_read = 0;
            let mut timestamp_arr;
            let start = SystemTime::now();
            loop {
                let bytes_read= stream.read(&mut buffer);
                if bytes_read > 0 {
                    total_bytes_read += bytes_read;
                    timestamp_arr = start.elapsed().expect("Invalid timestamp").as_millis().to_be_bytes();
                    buffer.insert(0, ch_id);
                    buffer.insert(1, ((MediaMessageId::MEDIA_MESSAGE_DATA as u16) >> 8) as u8);
                    buffer.insert(2, ((MediaMessageId::MEDIA_MESSAGE_DATA as u16) & 0xff) as u8);
                    for i in 0..8
                    {
                        buffer.insert(3 + i, timestamp_arr[8 + i]);
                    }
                    let pkt_rsp = Packet {
                        channel: ch_id,
                        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: buffer,
                    };
                    let _ = tx.send(pkt_rsp);
                }
                else {
                    info!( "{} Video stream finished", get_name());
                    break;
                }
            }
            let bytes_str = if total_bytes_read < 1024 {
                format!("{total_bytes_read}B")
            } else {
                format!("{}KiB", total_bytes_read / 1024)
            };
            info!("{} Read {} from client", get_name(), bytes_str);
        }
        println!("Listener thread exiting");
    }

}
pub async fn th_media_sink_audio_guidance(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>, acfg:AudioConfig)-> Result<()>{
    info!( "{}: Starting...", get_name());
    let mut audio_stream_started:bool=false;
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
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                    if rsp.status() != STATUS_SUCCESS
                    {
                        error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                    }
                    else {
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
                    }
                }
                else {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = CustomCommandMessage::parse_from_bytes(&data) {
                    if msg.cmd() == CustomCommand::CMD_OPEN_CH
                    {
                        let mut open_req = ChannelOpenRequest::new();
                        open_req.set_priority(0);
                        open_req.set_service_id(ch_id);
                        let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                        payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: ch_id as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                    }
                }
                else {
                    error!( "{} CustomCommandMessage couldn't be parsed",get_name());
                }
            }
            else if message_id == MEDIA_MESSAGE_CONFIG  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChConfig::parse_from_bytes(&data)
                {
                    info!( "{}, channel {:?}: Message status: {:?}", get_name(), pkt.channel, rsp.status());
                    if rsp.status() == STATUS_READY
                    {
                        info!( "{}, channel {:?}: Starting video capture", get_name(), pkt.channel);
                        if acfg.codec == MediaCodec::AUDIO_PCM
                        {
                            audio_stream_started =true;
                        }
                        else
                        {
                            error!( "{}: Unsupported audio codec detected", get_name());
                        }
                    }
                }
                else
                {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else
            {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }

    }
    fn get_name() -> String {
        let dev = "MediaSinkService Audio Guidance";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_media_sink_audio_streaming(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>, acfg:AudioConfig)-> Result<()>{
    info!( "{}: Starting...", get_name());
    let mut audio_stream_started:bool=false;
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
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                    if rsp.status() != STATUS_SUCCESS
                    {
                        error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                    }
                    else {
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
                    }
                }
                else {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = CustomCommandMessage::parse_from_bytes(&data) {
                    if msg.cmd() == CustomCommand::CMD_OPEN_CH
                    {
                        let mut open_req = ChannelOpenRequest::new();
                        open_req.set_priority(0);
                        open_req.set_service_id(ch_id);
                        let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                        payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: ch_id as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                    }
                }
                else {
                    error!( "{} CustomCommandMessage couldn't be parsed",get_name());
                }
            }
            else if message_id == MEDIA_MESSAGE_CONFIG  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChConfig::parse_from_bytes(&data)
                {
                    info!( "{}, channel {:?}: Message status: {:?}", get_name(), pkt.channel, rsp.status());
                    if rsp.status() == STATUS_READY
                    {
                        info!( "{}, channel {:?}: Starting video capture", get_name(), pkt.channel);
                        if acfg.codec == MediaCodec::AUDIO_PCM
                        {
                            audio_stream_started =true;
                        }
                        else
                        {
                            error!( "{}: Unsupported audio codec detected", get_name());
                        }
                    }
                }
                else
                {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else
            {
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
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = CustomCommandMessage::parse_from_bytes(&data) {
                    if msg.cmd() == CustomCommand::CMD_OPEN_CH
                    {
                        let mut open_req = ChannelOpenRequest::new();
                        open_req.set_priority(0);
                        open_req.set_service_id(ch_id);
                        let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                        payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: ch_id as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                    }
                }
                else {
                    error!( "{} CustomCommandMessage couldn't be parsed",get_name());
                }
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
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = CustomCommandMessage::parse_from_bytes(&data) {
                    if msg.cmd() == CustomCommand::CMD_OPEN_CH
                    {
                        let mut open_req = ChannelOpenRequest::new();
                        open_req.set_priority(0);
                        open_req.set_service_id(ch_id);
                        let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                        payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: ch_id as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                    }
                }
                else {
                    error!( "{} CustomCommandMessage couldn't be parsed",get_name());
                }
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
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = CustomCommandMessage::parse_from_bytes(&data) {
                    if msg.cmd() == CustomCommand::CMD_OPEN_CH
                    {
                        let mut open_req = ChannelOpenRequest::new();
                        open_req.set_priority(0);
                        open_req.set_service_id(ch_id);
                        let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                        payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: ch_id as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                    }
                }
                else {
                    error!( "{} CustomCommandMessage couldn't be parsed",get_name());
                }
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