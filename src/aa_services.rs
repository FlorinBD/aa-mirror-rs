//! This crate provides service implementation for  [Android Open Accessory Protocol 1.0](https://source.android.com/devices/accessories/aoa)

use simplelog::*;
use std::fmt;
use std::time::{Duration, SystemTime};
use futures::future::err;
use futures_timer::Delay;
use tokio::sync::mpsc::{Receiver, Sender};
use serde::{Serialize, Deserialize};

// protobuf stuff:
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use crate::aa_services::protos::Config as ChConfig;
use crate::aa_services::protos::ConfigStatus;
use crate::aa_services::protos::ConfigStatus::*;
use crate::aa_services::sensor_source_service::Sensor;
use crate::aa_services::ByeByeReason::USER_SELECTION;
use crate::aa_services::MessageStatus::*;
use crate::aa_services::MediaMessageId::*;
use crate::aa_services::InputMessageId::*;
use crate::aa_services::SensorMessageId::*;
//use crate::aa_services::SensorType::*;
use crate::aa_services::MediaCodecType::*;
use protobuf::{Message};
//use tokio::sync::broadcast;
use tokio_uring::net::{TcpStream, TcpListener};
use protos::*;
use protos::ControlMessageType::{self, *};
use crate::adb;
use crate::channel_manager::{Packet, ENCRYPTED, FRAME_TYPE_CONTROL, FRAME_TYPE_FIRST, FRAME_TYPE_LAST};
use crate::config::{HU_CONFIG_DELAY_MS, SCRCPY_PORT};
use crate::scrcpy::ScrcpyControlMessageType;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Copy, Clone, Debug)]
pub enum ServiceType
{
    None,
    InputSource,
    MediaSink,
    MediaSource,
    SensorSource,
    VendorExtension,
    Bluetooth,
}

#[derive(Copy, Clone, Debug)]
pub enum AAMessageType
{
    Unknown=-1,
    Control=0,
    Input,
    Media,
    Sensor,
    VendorExtension,
    Bluetooth,
}

impl Default for ServiceType {
    fn default() -> Self { ServiceType::None }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ProjectionStatus
{
    TransitionToFS,
    FirstScreen,
    TransitionToProjected,
    ProjectedRecording,
    ProjectedPause
}
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CommandState
{
    NotDone,
    InProgress,
    Done
}
impl Default for CommandState {
    fn default() -> Self { CommandState::NotDone }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct ServiceStatus {
    pub open_ch_cmd: CommandState,
    pub service_type: ServiceType,
    pub ch_id:i32,
    ///Send Setup and all other subsequent commands
    pub enabled:bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct VideoStreamingParams {
    pub(crate) bitrate: i32,
    pub(crate) res_w:i32,
    pub(crate) res_h:i32,
    pub(crate) fps:i32,
    pub(crate) dpi:i32,
    pub(crate) sid:u8,
    pub(crate) max_unack:u32,
}

#[derive(Serialize, Deserialize, Clone,Debug)]
pub(crate) struct AudioStreamingParams {
    pub(crate) codec: MediaCodec,
    pub(crate) bitrate: i32,
    pub(crate) sid:u8,
    pub(crate) max_unack:u32,
}

impl Default for VideoStreamingParams {
    fn default() -> Self {
        Self {
            bitrate: 4_000_000,//8Mb by default but is too much
            res_w: 800,
            res_h: 480,
            fps: 60,
            dpi: 160,
            sid:0,
            max_unack:0,
        }
    }
}

impl Default for AudioStreamingParams {
    fn default() -> Self {
        Self {
            bitrate: 4800,
            sid:0,
            max_unack:0,
            codec: MediaCodec::AUDIO_PCM,
        }
    }
}
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum SensorType {
    SENSOR_LOCATION = 1,
    SENSOR_COMPASS = 2,
    SENSOR_SPEED = 3,
    SENSOR_RPM = 4,
    SENSOR_ODOMETER = 5,
    SENSOR_FUEL = 6,
    SENSOR_PARKING_BRAKE = 7,
    SENSOR_GEAR = 8,
    SENSOR_OBDII_DIAGNOSTIC_CODE = 9,
    SENSOR_NIGHT_MODE = 10,
    SENSOR_ENVIRONMENT_DATA = 11,
    SENSOR_HVAC_DATA = 12,
    SENSOR_DRIVING_STATUS_DATA = 13,
    SENSOR_DEAD_RECKONING_DATA = 14,
    SENSOR_PASSENGER_DATA = 15,
    SENSOR_DOOR_DATA = 16,
    SENSOR_LIGHT_DATA = 17,
    SENSOR_TIRE_PRESSURE_DATA = 18,
    SENSOR_ACCELEROMETER_DATA = 19,
    SENSOR_GYROSCOPE_DATA = 20,
    SENSOR_GPS_SATELLITE_DATA = 21,
    SENSOR_TOLL_CARD = 22,
    SENSOR_VEHICLE_ENERGY_MODEL_DATA = 23,
    SENSOR_TRAILER_DATA = 24,
}

impl TryFrom<i32> for SensorType {
    type Error = ();

    fn try_from(v: i32) -> std::result::Result<Self, Self::Error> {
        match v {
            1 => Ok(SensorType::SENSOR_LOCATION),
            2 => Ok(SensorType::SENSOR_COMPASS),
            3 => Ok(SensorType::SENSOR_SPEED),
            4 => Ok(SensorType::SENSOR_RPM),
            5 => Ok(SensorType::SENSOR_ODOMETER),
            6 => Ok(SensorType::SENSOR_FUEL),
            7 => Ok(SensorType::SENSOR_PARKING_BRAKE),
            8 => Ok(SensorType::SENSOR_GEAR),
            9 => Ok(SensorType::SENSOR_OBDII_DIAGNOSTIC_CODE),
            10 => Ok(SensorType::SENSOR_NIGHT_MODE),
            11 => Ok(SensorType::SENSOR_ENVIRONMENT_DATA),
            12 => Ok(SensorType::SENSOR_HVAC_DATA),
            13 => Ok(SensorType::SENSOR_DRIVING_STATUS_DATA),
            14 => Ok(SensorType::SENSOR_DEAD_RECKONING_DATA),
            15 => Ok(SensorType::SENSOR_PASSENGER_DATA),
            16 => Ok(SensorType::SENSOR_DOOR_DATA),
            17 => Ok(SensorType::SENSOR_LIGHT_DATA),
            18 => Ok(SensorType::SENSOR_TIRE_PRESSURE_DATA),
            19 => Ok(SensorType::SENSOR_ACCELEROMETER_DATA),
            20 => Ok(SensorType::SENSOR_GYROSCOPE_DATA),
            21 => Ok(SensorType::SENSOR_GPS_SATELLITE_DATA),
            22 => Ok(SensorType::SENSOR_TOLL_CARD),
            23 => Ok(SensorType::SENSOR_VEHICLE_ENERGY_MODEL_DATA),
            24 => Ok(SensorType::SENSOR_TRAILER_DATA),
            _ => Err(()),
        }
    }
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
#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
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
    pub bitrate:u32,
    pub channels:u32,
    pub bitdepth:u32,
}
impl fmt::Display for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
        // or, alternatively:
        // fmt::Debug::fmt(self, f)
    }
}
pub async fn th_sensor_source(ch_id: i32, enabled:bool, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>, sensors: Vec<SensorType>) -> Result<()> {
    info!( "{}: Starting...", get_name());
    let mut prev_nt_mode=false;
    loop {
        let pkt = rx_srv.recv().await.ok_or("service reader channel hung up")?;
        if pkt.channel != ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded", get_name(), pkt.channel);
        } else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
            {
                info!("{} Received message id: {}", get_name(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                    if rsp.status() != STATUS_SUCCESS
                    {
                        error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                    }
                    else
                    {
                        if sensors.contains(&SensorType::SENSOR_NIGHT_MODE) {
                            info!("{} send SENSOR_MESSAGE_REQUEST",get_name());
                            let mut req = SensorRequest::new();
                            req.set_type(protos::SensorType::SENSOR_NIGHT_MODE);
                            req.set_min_update_period(1_000_000_000);
                            let mut payload: Vec<u8>=Vec::new();
                            payload.extend_from_slice(&(SensorMessageId::SENSOR_MESSAGE_REQUEST as u16).to_be_bytes());
                            payload.extend_from_slice(&(req.write_to_bytes()?));

                            let pkt_rsp = Packet {
                                channel: ch_id as u8,
                                flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                final_length: None,
                                payload: payload,
                            };
                            if let Err(_) = tx_srv.send(pkt_rsp).await
                            {
                                error!( "{} mpsc send error", get_name());
                            };
                        }

                    }
                }
                else {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == SENSOR_MESSAGE_RESPONSE  as i32
            {
                info!("{} Received message SENSOR_MESSAGE_RESPONSE", get_name());
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = SensorResponse::parse_from_bytes(&data) {
                    if rsp.status() != STATUS_SUCCESS
                    {
                        error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                    }
                }
            }
            else if message_id == SENSOR_MESSAGE_BATCH  as i32
            {
                info!("{} Received message SENSOR_MESSAGE_BATCH", get_name());
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = SensorBatch::parse_from_bytes(&data) {
                    if !rsp.night_mode_data.is_empty()
                    {
                        if let Some(night) = rsp.night_mode_data.first() {
                            let value = night.night_mode.unwrap_or(false);
                            if value != prev_nt_mode
                            {
                                prev_nt_mode=value;
                                info!("{} Switching theme for MD, night: {}", get_name(), value);
                                let mut mode="yes";
                                if !value{
                                    mode="no";
                                }
                                let mut cmd_shell:Vec<String> = vec![];
                                cmd_shell.push("cmd".to_string());
                                cmd_shell.push("uimode".to_string());
                                cmd_shell.push("night".to_string());
                                cmd_shell.push(format!("{}",mode.to_string() ));
                                let (mut shell, mut sh_reader,line)=adb::shell_cmd(cmd_shell).await?;
                                info!("{} ADB cmd shell response: {:?}",get_name(), line);
                                if !line.contains("Night mode:") && shell.id().is_some()
                                {
                                    error!( "{} error switching MD theme", get_name());
                                }
                                shell.kill().await?;
                            }
                        }
                    }
                }
                else {
                    error!( "{} error deserializing SensorBatch", get_name());
                }
            }
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                if cmd == CustomCommand::CMD_OPEN_CH as i32
                {
                    let mut open_req = ChannelOpenRequest::new();
                    open_req.set_priority(0);
                    open_req.set_service_id(ch_id);
                    let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                    payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                    payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: ch_id as u8,
                        flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    //tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                    if let Err(_) = tx_srv.send(pkt_rsp).await
                    {
                        error!( "{} mpsc send error", get_name());
                    };
                }
                else if cmd == CustomCommand::MD_DISCONNECTED as i32
                {
                    info!( "{} MD_DISCONNECTED received", get_name());
                }
            }
            else
            {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }
    }
    fn get_name() -> String {
        let dev = "SensorSourceService";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_media_sink_video(ch_id: i32, enabled:bool, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>, scrcpy_cmd: flume::Sender<Packet>, mut video_params:VideoStreamingParams, dhu:bool) -> Result<()>{

    info!( "{}: Starting...", get_name());
    //let mut md_connected=false;
    let mut projection_state=ProjectionStatus::TransitionToProjected;
    let mut video_focus=false;
    let mut config_recived=false;
    let mut session_id=0;
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
                        if enabled
                        {
                            video_setup(&tx_srv, ch_id as u8).await?;
                        }
                    }
                }
                else {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == ControlMessageType::MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                if cmd == CustomCommand::CMD_OPEN_CH as i32
                {
                    let mut open_req = ChannelOpenRequest::new();
                    open_req.set_priority(0);
                    open_req.set_service_id(ch_id);
                    let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                    payload.insert(0, ((ControlMessageType::MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                    payload.insert(1, ((ControlMessageType::MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: ch_id as u8,
                        flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = tx_srv.send(pkt_rsp).await{
                        error!( "{} response send error",get_name());
                    };
                }
                else if cmd == CustomCommand::MD_DISCONNECTED as i32 {
                    info!("{} MD disconnected, send media STOP to HU",get_name());
                    stop_media(&tx_srv, ch_id as u8).await?;
                    video_setup(&tx_srv, ch_id as u8).await?;
                }
            }
            else if message_id == MediaMessageId::MEDIA_MESSAGE_CONFIG  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChConfig::parse_from_bytes(&data)
                {
                    info!( "{}, channel {:?} MEDIA_MESSAGE_CONFIG received: Message status: {:?}, max_unack: {}", get_name(), pkt.channel, rsp.status(), rsp.max_unacked());
                    if rsp.status() == ConfigStatus::STATUS_READY
                    {
                        config_recived=true;
                        video_params.max_unack=rsp.max_unacked();
                    }
                }
                else
                {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == MediaMessageId::MEDIA_MESSAGE_VIDEO_FOCUS_NOTIFICATION  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = VideoFocusNotification::parse_from_bytes(&data)
                {
                    info!( "{}, channel {:?}: Message status: {:?}", get_name(), pkt.channel, rsp.focus());
                    if (rsp.focus() == VideoFocusMode::VIDEO_FOCUS_PROJECTED) || (rsp.focus()==VideoFocusMode::VIDEO_FOCUS_PROJECTED_NO_INPUT_FOCUS)
                    {
                        info!( "{}, channel {:?}: VIDEO_FOCUS_PROJECTED received", get_name(), pkt.channel);
                        video_focus=true;

                        if projection_state==ProjectionStatus::TransitionToProjected
                        {
                            session_id +=1;
                            start_media(&tx_srv, ch_id as u8, session_id).await?;
                            start_scrcpy_media(&scrcpy_cmd, ch_id as u8, &video_params).await?;
                            projection_state=ProjectionStatus::ProjectedRecording;
                        }
                        else if projection_state==ProjectionStatus::ProjectedPause
                        {
                            resume_scrcpy_media(&scrcpy_cmd, ch_id as u8).await?;
                            session_id +=1;
                            start_media(&tx_srv, ch_id as u8, session_id).await?;
                            projection_state=ProjectionStatus::ProjectedRecording;
                        }
                        else
                        {
                            debug!("{}, channel {:?}: video streaming already started, ignoring packet", get_name(), pkt.channel);
                        }
                    }
                    else
                    {
                        video_focus=false;
                        debug!( "{} video focus lost",get_name());
                        if projection_state==ProjectionStatus::ProjectedRecording
                        {
                            pause_scrcpy_media(&scrcpy_cmd, ch_id as u8).await?;
                            stop_media(&tx_srv, ch_id as u8).await?;
                            projection_state=ProjectionStatus::ProjectedPause;
                        }
                    }
                }
                else
                {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == MediaMessageId::MEDIA_MESSAGE_START  as i32//HU send this response as confirmation to START from MD, but only if STOP was sent before START
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                info!( "{}, channel {:?}: MEDIA_MESSAGE_START received", get_name(), pkt.channel);
            }
            else if message_id == MediaMessageId::MEDIA_MESSAGE_STOP  as i32
            {
                error!( "{}, channel {:?}: MEDIA_MESSAGE_STOP received but not managed", get_name(), pkt.channel);

            }
            else if message_id == MediaMessageId::MEDIA_MESSAGE_ACK  as i32//now this is done by PacketProxy, not needed
            {
                error!("{}: Media ACK received by service, was not handled by PacketProxy", get_name())
                /*if video_stream_started
                {

                    if let Err(_) = scrcpy_cmd.send_async(pkt).await{
                        error!( "{} mpsc send error",get_name());
                    };

                }*/
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

    async fn show_first_screen(tx: &Sender<Packet>, ch_id: u8, cfg_frame:&Vec<u8>, first_frame:&Vec<u8>)
    {
        info!( "{}, channel {:?}: Showing startup screen", get_name(), ch_id);
        //Send config frame
        let mut payload = Vec::with_capacity(cfg_frame.len() + 2);
        payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16).to_be_bytes());
        payload.extend_from_slice(&cfg_frame);

        let pkt_rsp = Packet {
            channel: ch_id,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };

        if let Err(_) = tx.send(pkt_rsp).await{
            error!( "{} mpsc send error",get_name());
        };
        //Send first frame
        let mut payload = Vec::with_capacity(first_frame.len() + 10);
        payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_DATA as u16).to_be_bytes());
        payload.extend_from_slice(&0u64.to_be_bytes());
        payload.extend_from_slice(&first_frame);
        let pkt_rsp = Packet {
            channel: ch_id,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = tx.send(pkt_rsp).await{
            error!( "{} mpsc send error",get_name());
        };
    }

    async fn video_setup(tx: &Sender<Packet>, ch_id: u8)->Result<()> {
        info!( "{}, channel {:?}: Sending SETUP command", get_name(), ch_id);
        let mut media_setup= Setup::new();
        media_setup.set_type(MediaCodecType::MEDIA_CODEC_VIDEO_H264_BP);
        let mut payload: Vec<u8>=Vec::new();
        payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_SETUP as u16).to_be_bytes());
        payload.extend_from_slice(&(media_setup.write_to_bytes()?));
        let pkt_rsp = Packet {
            channel: ch_id,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = tx.send(pkt_rsp).await{
            error!( "{} send error",get_name());
        };
        Ok(())
    }
    async fn start_media(tx: &Sender<Packet>, ch_id: u8, session_id:i32)->Result<()> {
        info!( "{}, channel {:?}: Sending START command, session id= {}", get_name(), ch_id, session_id);
        let mut start_req = Start::new();
        start_req.set_session_id(session_id);
        start_req.set_configuration_index(0);
        let mut payload: Vec<u8> = start_req.write_to_bytes().expect("serialization failed");
        payload.insert(0, ((MediaMessageId::MEDIA_MESSAGE_START as u16) >> 8) as u8);
        payload.insert(1, ((MediaMessageId::MEDIA_MESSAGE_START as u16) & 0xff) as u8);

        let pkt_rsp = Packet {
            channel: ch_id,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = tx.send(pkt_rsp).await{
            error!( "{} response send error",get_name());
        };
        Ok(())
    }

    async fn stop_media(tx: &Sender<Packet>, ch_id: u8)->Result<()> {
        info!( "{}, channel {:?}: Sending STOP command", get_name(), ch_id);
        let media_stop= Stop::new();
        let mut payload: Vec<u8>=Vec::new();
        payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_STOP as u16).to_be_bytes());
        payload.extend_from_slice(&(media_stop.write_to_bytes()?));
        let pkt_rsp = Packet {
            channel: ch_id,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = tx.send(pkt_rsp).await{
            error!( "{} send error",get_name());
        };
        Ok(())
    }

    async fn start_scrcpy_media(tx: &flume::Sender<Packet>, ch_id: u8, vp:&VideoStreamingParams)->Result<()> {
        debug!( "{}, Starting video streaming", get_name());
        let bytes: Vec<u8> = postcard::to_stdvec(vp)?;
        let mut payload = Vec::new();
        payload.extend_from_slice(&(MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
        payload.extend_from_slice(&(CustomCommand::CMD_START_VIDEO_RECORDING as u16).to_be_bytes());
        payload.extend_from_slice(&bytes);

        let pkt_rsp = Packet {
            channel: ch_id,
            flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload.clone(),
        };
        if let Err(_) = tx.send_async(pkt_rsp).await{
            error!( "{} mpsc send error",get_name());
        };
        Ok(())
    }

    async fn pause_scrcpy_media(tx: &flume::Sender<Packet>, ch_id: u8)->Result<()> {
        debug!( "{}, Pausing video streaming", get_name());
        let mut payload = Vec::new();
        payload.extend_from_slice(&(MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
        payload.extend_from_slice(&(CustomCommand::CMD_PAUSE_VIDEO_RECORDING as u16).to_be_bytes());

        let pkt_rsp = Packet {
            channel: ch_id,
            flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload.clone(),
        };
        if let Err(_) = tx.send_async(pkt_rsp).await{
            error!( "{} mpsc send error",get_name());
        };
        Ok(())
    }
    async fn resume_scrcpy_media(tx: &flume::Sender<Packet>, ch_id: u8)->Result<()> {
        debug!( "{}, Resuming video streaming", get_name());
        let mut payload = Vec::new();
        payload.extend_from_slice(&(MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
        payload.extend_from_slice(&(CustomCommand::CMD_RESUME_VIDEO_RECORDING as u16).to_be_bytes());

        let pkt_rsp = Packet {
            channel: ch_id,
            flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload.clone(),
        };
        if let Err(_) = tx.send_async(pkt_rsp).await{
            error!( "{} mpsc send error",get_name());
        };
        Ok(())
    }
}
pub async fn th_media_sink_audio_guidance(ch_id: i32, enabled:bool, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>, acfg:AudioConfig)-> Result<()>{
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
                    else
                    {
                        if enabled
                        {
                            let mut cfg_req= Setup::new();
                            if acfg.codec == MediaCodec::AUDIO_PCM
                            {
                                cfg_req.set_type(MediaCodecType::MEDIA_CODEC_AUDIO_PCM);
                            }
                            else if acfg.codec == MediaCodec::AUDIO_AAC_LC
                            {
                                cfg_req.set_type(MediaCodecType::MEDIA_CODEC_AUDIO_AAC_LC);
                            }
                            else {
                                error!("{}: Unsupported audio codec detected", get_name())
                            }

                            let mut payload: Vec<u8>=cfg_req.write_to_bytes().expect("serialization failed");
                            payload.insert(0,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) >> 8) as u8);
                            payload.insert( 1,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) & 0xff) as u8);

                            let pkt_rsp = Packet {
                                channel: ch_id as u8,
                                flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                final_length: None,
                                payload: payload,
                            };
                            //tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                            if let Err(_) = tx_srv.send(pkt_rsp).await{
                                error!( "{} mpsc send error",get_name());
                            };
                        }
                    }
                }
                else {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                info!("{} Received {} message", ch_id.to_string(), message_id);
                if cmd == CustomCommand::CMD_OPEN_CH as i32
                {
                    let mut open_req = ChannelOpenRequest::new();
                    open_req.set_priority(0);
                    open_req.set_service_id(ch_id);
                    let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                    payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                    payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: ch_id as u8,
                        flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = tx_srv.send(pkt_rsp).await {
                        error!( "{} response send error",get_name());
                    };
                }
                else if (cmd == CustomCommand::CMD_SETUP_CH as i32) && enabled
                {
                    /*let mut cfg_req= Setup::new();
                    cfg_req.set_type(MediaCodecType::MEDIA_CODEC_AUDIO_PCM);

                    let mut payload: Vec<u8>=cfg_req.write_to_bytes().expect("serialization failed");
                    payload.insert(0,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) >> 8) as u8);
                    payload.insert( 1,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: ch_id as u8,
                        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    tx_srv.send(pkt_rsp).await.expect("TODO: panic message");*/
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
                        info!( "{}, channel {:?}: Starting audio capture", get_name(), pkt.channel);
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
            else if message_id == MediaMessageId::MEDIA_MESSAGE_ACK  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
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
pub async fn th_media_sink_audio_streaming(ch_id: i32, enabled:bool, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>, scrcpy_cmd: flume::Sender<Packet>, acfg:AudioConfig, mut audio_params:AudioStreamingParams) -> Result<()>{
    info!( "{}: Starting...", get_name());
    let mut audio_stream_started:bool=false;
    let mut audio_stream_paused=false;
    //let mut md_connected=false;
    let mut audio_focus=false;
    let mut config_recived=false;
    let mut session_id=1;
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
                        audio_focus=true;
                        if enabled
                        {
                            let mut cfg_req= Setup::new();
                            if acfg.codec == MediaCodec::AUDIO_PCM
                            {
                                cfg_req.set_type(MediaCodecType::MEDIA_CODEC_AUDIO_PCM);
                            }
                            else if acfg.codec == MediaCodec::AUDIO_AAC_LC
                            {
                                cfg_req.set_type(MediaCodecType::MEDIA_CODEC_AUDIO_AAC_LC);
                            }
                            else {
                                error!("{}: Unsupported audio codec detected", get_name())
                            }

                            let mut payload: Vec<u8>=cfg_req.write_to_bytes().expect("serialization failed");
                            payload.insert(0,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) >> 8) as u8);
                            payload.insert( 1,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) & 0xff) as u8);

                            let pkt_rsp = Packet {
                                channel: ch_id as u8,
                                flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                final_length: None,
                                payload: payload,
                            };
                            //tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                            if let Err(_) = tx_srv.send(pkt_rsp).await{
                                error!( "{} mpsc send error",get_name());
                            };
                        }
                    }
                }
                else {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                if cmd == CustomCommand::CMD_OPEN_CH as i32
                {
                    let mut open_req = ChannelOpenRequest::new();
                    open_req.set_priority(0);
                    open_req.set_service_id(ch_id);
                    let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                    payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                    payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: ch_id as u8,
                        flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = tx_srv.send(pkt_rsp).await{
                        error!( "{} response send error",get_name());
                    };
                }
                else if cmd == CustomCommand::MD_DISCONNECTED as i32 {
                    debug!("{} MD diconnected",get_name());
                    stop_media(&tx_srv, ch_id as u8).await?;
                }
            }
            else if message_id == MEDIA_MESSAGE_CONFIG  as i32
            {
                info!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChConfig::parse_from_bytes(&data)
                {
                    info!( "{}, channel {:?} MEDIA_MESSAGE_CONFIG received: Message status: {:?}, max_unack: {}", get_name(), pkt.channel, rsp.status(), rsp.max_unacked());
                    if rsp.status() == STATUS_READY
                    {
                        config_recived=true;
                        audio_params.max_unack=rsp.max_unacked();
                        //info!( "{}, channel {:?}: Starting audio capture", get_name(), pkt.channel);
                        if (acfg.codec == MediaCodec::AUDIO_PCM) || (acfg.codec == MediaCodec::AUDIO_AAC_LC)
                        {
                            session_id +=1;
                            start_media(&tx_srv, ch_id as u8, session_id).await?;
                            audio_stream_started =true;
                            info!( "{} Send custom CMD_START_AUDIO_RECORDING for ch {}",get_name(), ch_id);
                            let bytes: Vec<u8> = postcard::to_stdvec(&audio_params)?;
                            let mut payload = Vec::new();
                            payload.extend_from_slice(&(MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                            payload.extend_from_slice(&(CustomCommand::CMD_START_AUDIO_RECORDING as u16).to_be_bytes());
                            payload.extend_from_slice(&bytes);

                            let pkt_rsp = Packet {
                                channel: ch_id as u8,
                                flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                final_length: None,
                                payload: payload.clone(),
                            };
                            scrcpy_cmd.send_async(pkt_rsp).await?;
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
            else if message_id == MediaMessageId::MEDIA_MESSAGE_ACK  as i32 //now this is done by PacketProxy, not needed
            {
                error!("{}: Media ACK received by service, was not handled by PacketProxy", get_name())
                /*if audio_stream_started
                {
                    //info!("{} Received {} message, proxy to SCRCPY", ch_id.to_string(), message_id);
                    scrcpy_cmd.send_async(pkt).await?;
                }*/
            }
            else if message_id == MediaMessageId::MEDIA_MESSAGE_AUDIO_UNDERFLOW_NOTIFICATION  as i32
            {
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(_) = AudioUnderflowNotification::parse_from_bytes(&data)
                {
                    debug!("{} Received {} message: MEDIA_MESSAGE_AUDIO_UNDERFLOW_NOTIFICATION", ch_id.to_string(), message_id);
                }
                else {
                    error!("{}: Unable to deserialize AudioUnderflowNotification", ch_id.to_string())
                }
            }
            else if message_id == ControlMessageType::MESSAGE_AUDIO_FOCUS_NOTIFICATION as i32
            {
                //Proxy msg from Control channel
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = AudioFocusNotification::parse_from_bytes(&data) {
                    debug!("{} Received {} message: MESSAGE_AUDIO_FOCUS_NOTIFICATION", ch_id.to_string(), message_id);
                    if (msg.focus_state() == AudioFocusStateType::AUDIO_FOCUS_STATE_GAIN) || (msg.focus_state() == AudioFocusStateType::AUDIO_FOCUS_STATE_GAIN_TRANSIENT) || (msg.focus_state() == AudioFocusStateType::AUDIO_FOCUS_STATE_GAIN_MEDIA_ONLY)
                    {
                        audio_focus=true;
                        if audio_stream_started
                        {
                            if audio_stream_paused
                            {
                                debug!("{}: Resuming audio stream", get_name());
                                audio_stream_paused=false;
                                start_media(&tx_srv, ch_id as u8, session_id).await?;
                                let mut payload = Vec::new();
                                payload.extend_from_slice(&(MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                                payload.extend_from_slice(&(CustomCommand::CMD_RESUME_AUDIO_RECORDING as u16).to_be_bytes());

                                let pkt_rsp = Packet {
                                    channel: ch_id as u8,
                                    flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                    final_length: None,
                                    payload: payload.clone(),
                                };
                                if let Err(_) = scrcpy_cmd.send_async(pkt_rsp).await{
                                    error!( "{} mpsc send error",get_name());
                                };
                            }
                        }
                        else {
                            error!("{}: Audio stream not started, ignoring message", get_name());
                        }
                    }
                    else {
                        //focus lost
                        audio_focus=false;
                        if audio_stream_started
                        {
                            if !audio_stream_paused
                            {
                                debug!("{}: Pausing audio stream", get_name());
                                audio_stream_paused=true;
                                stop_media(&tx_srv, ch_id as u8).await?;
                                let mut payload = Vec::new();
                                payload.extend_from_slice(&(MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                                payload.extend_from_slice(&(CustomCommand::CMD_PAUSE_AUDIO_RECORDING as u16).to_be_bytes());

                                let pkt_rsp = Packet {
                                    channel: ch_id as u8,
                                    flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                    final_length: None,
                                    payload: payload.clone(),
                                };
                                if let Err(_) = scrcpy_cmd.send_async(pkt_rsp).await{
                                    error!( "{} mpsc send error",get_name());
                                };
                            }
                        }
                    }
                }
                else {
                    error!("{}: Unable to deserialize AudioFocusNotification", ch_id.to_string())
                }
            }
            else
            {
                error!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }

    }

    async fn stop_media(tx: &Sender<Packet>, ch_id: u8)->Result<()> {
        info!( "{}, channel {:?}: Sending STOP command", get_name(), ch_id);
        let media_stop= Stop::new();
        let mut payload: Vec<u8>=Vec::new();
        payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_STOP as u16).to_be_bytes());
        payload.extend_from_slice(&(media_stop.write_to_bytes()?));
        let pkt_rsp = Packet {
            channel: ch_id,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = tx.send(pkt_rsp).await{
            error!( "{} send error",get_name());
        };
        Ok(())
    }
    async fn start_media(tx: &Sender<Packet>, ch_id: u8, session_id:i32)->Result<()> {
        info!( "{}, channel {:?}: Sending START command", get_name(), ch_id);
        let mut start_req = Start::new();
        start_req.set_session_id(session_id);
        start_req.set_configuration_index(0);
        let mut payload: Vec<u8> = start_req.write_to_bytes().expect("serialization failed");
        payload.insert(0, ((MediaMessageId::MEDIA_MESSAGE_START as u16) >> 8) as u8);
        payload.insert(1, ((MediaMessageId::MEDIA_MESSAGE_START as u16) & 0xff) as u8);

        let pkt_rsp = Packet {
            channel: ch_id,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = tx.send(pkt_rsp).await{
            error!( "{} response send error",get_name());
        };
        Ok(())
    }
    fn get_name() -> String {
        let dev = "MediaSinkService Audio Streaming";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_media_source(ch_id: i32, enabled:bool, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()>{
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
                let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                if cmd == CustomCommand::CMD_OPEN_CH as i32
                {
                    let mut open_req = ChannelOpenRequest::new();
                    open_req.set_priority(0);
                    open_req.set_service_id(ch_id);
                    let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                    payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                    payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: ch_id as u8,
                        flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = tx_srv.send(pkt_rsp).await{
                        error!( "{} response send error",get_name());
                    };
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
pub async fn th_input_source(ch_id: i32, enabled:bool, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>, scrcpy_cmd: flume::Sender<Packet>, keys:Vec<i32>)-> Result<()>{
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
                debug!("{} Received {} message", ch_id.to_string(), message_id);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                    if rsp.status() != STATUS_SUCCESS
                    {
                        error!( "{}, channel {:?}: Wrong message status received", get_name(), pkt.channel);
                    }
                    else {
                        //FIXME send BindingRequest

                        let mut binding_req = KeyBindingRequest::new();
                        binding_req.keycodes.extend_from_slice(&keys);

                        let mut payload: Vec<u8>=Vec::new();
                        payload.extend_from_slice(&(InputMessageId::INPUT_MESSAGE_KEY_BINDING_REQUEST as u16).to_be_bytes());
                        payload.extend_from_slice(&binding_req.write_to_bytes().expect("serialization failed"));

                        let pkt_rsp = Packet {
                            channel: ch_id as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        if let Err(_) = tx_srv.send(pkt_rsp).await{
                            error!( "{} response send error",get_name());
                        };
                    }
                }
                else {
                    error!( "{}, channel {:?}: Unable to parse received message", get_name(), pkt.channel);
                }
            }
            else if message_id == MESSAGE_CUSTOM_CMD  as i32
            {
                debug!("{} Received {} message", ch_id.to_string(), message_id);
                let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                if cmd == CustomCommand::CMD_OPEN_CH as i32
                {
                    let mut open_req = ChannelOpenRequest::new();
                    open_req.set_priority(0);
                    open_req.set_service_id(ch_id);
                    let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                    payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                    payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: ch_id as u8,
                        flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = tx_srv.send(pkt_rsp).await{
                        error!( "{} response send error",get_name());
                    };
                }
            }
            else if message_id == InputMessageId::INPUT_MESSAGE_INPUT_REPORT  as i32
            {
                debug!("{} Received {} message, proxy to SCRCPY control channel", ch_id.to_string(), message_id);
                if let Err(_) = scrcpy_cmd.send_async(pkt).await{
                    error!( "{} scrcpy_cmd send error",get_name());
                };
                //tokio::task::yield_now().await;
            }
            else if message_id == InputMessageId::INPUT_MESSAGE_KEY_BINDING_RESPONSE  as i32
            {
                debug!("{} Received INPUT_MESSAGE_KEY_BINDING_RESPONSE", get_name());
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if  let Ok(rsp) = KeyBindingResponse::parse_from_bytes(&data) {
                    debug!("{} Decoded KeyBindingResponse status: {:?}",get_name(), rsp.status())
                }

            }
            else {
                error!( "{} Unmanaged message ID: {} received", get_name(), message_id);
            }
        }
    }
    fn get_name() -> String {
        let dev = "InputSourceService";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
pub async fn th_vendor_extension(ch_id: i32, enabled:bool, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()>{
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
                let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                if cmd == CustomCommand::CMD_OPEN_CH as i32
                {
                    let mut open_req = ChannelOpenRequest::new();
                    open_req.set_priority(0);
                    open_req.set_service_id(ch_id);
                    let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                    payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                    payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: ch_id as u8,
                        flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = tx_srv.send(pkt_rsp).await{
                        error!( "{} response send error",get_name());
                    };
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
pub async fn th_bluetooth(ch_id: i32, enabled:bool, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()>{
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
                let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                if cmd == CustomCommand::CMD_OPEN_CH as i32
                {
                    let mut open_req = ChannelOpenRequest::new();
                    open_req.set_priority(0);
                    open_req.set_service_id(ch_id);
                    let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                    payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                    payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: ch_id as u8,
                        flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = tx_srv.send(pkt_rsp).await{
                        error!( "{} response send error",get_name());
                    };
                }
            }
            else {
                info!( "{} Unknown message ID: {} received", get_name(), message_id);
            }
        }
    }
    fn get_name() -> String {
        let dev = "BluetoothService";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}
