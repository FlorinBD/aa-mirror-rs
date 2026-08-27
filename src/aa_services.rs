//! This crate provides service implementation for  [Android Open Accessory Protocol 1.0](https://source.android.com/devices/accessories/aoa)

use simplelog::*;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, SystemTime};
use anyhow::anyhow;
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
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
//use tokio::sync::broadcast;
use tokio_uring::net::{TcpStream, TcpListener};
use tokio_util::sync::CancellationToken;
use protos::*;
use protos::ControlMessageType::{self, *};
use crate::aa_services::MediaCodec::{AUDIO_AAC_LC, AUDIO_AAC_LC_ADTS, AUDIO_PCM, VIDEO_H264_BP, VIDEO_H265};
use crate::aa_services::VideoCodecResolution::{Video_1080x1920, Video_720x1280, Video_800x480};
use crate::aa_services::VideoFPS::{FPS_30, FPS_60};
use crate::adb;
use crate::channel_manager::{pkt_debug, Packet, TlsPacketProxy, ENCRYPTED, FRAME_TYPE_CONTROL, FRAME_TYPE_FIRST, FRAME_TYPE_LAST};
use crate::config::{AppConfig, HU_CONFIG_DELAY_MS, SCRCPY_PORT};
use crate::config_types::HexdumpLevel;
use crate::io_uring::{Endpoint, IoDevice};
use crate::scrcpy::{AudioServerState, ControlServerState, ScrcpyControlMessageType, ScrcpySize, VideoServerState};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Copy, Clone, Debug)]
pub enum ServiceType
{
    None,
    Control,
    InputSource,
    MediaSink,
    MediaSource,
    SensorSource,
    VendorExtension,
    Bluetooth,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AAMessageType
{
    Unknown=-1,
    Control=0,
    Input,
    Media,
    Sensor,
    VendorExtension,
    Bluetooth,
    WiFiProjection,
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
#[derive(Clone)]
pub struct AudioConfig
{
    pub codec: MediaCodec,
    pub stream_type: AudioStream,
    pub bitrate:u32,
    pub channels:u32,
    pub bitdepth:u32,
}
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            codec: MediaCodec::AUDIO_PCM,
            stream_type:AudioStream::MEDIA,
            bitrate: 1_000_000,
            channels:2,
            bitdepth: 16,
        }
    }
}
impl fmt::Display for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
        // or, alternatively:
        // fmt::Debug::fmt(self, f)
    }
}
#[derive(Clone)]
pub struct  AAService {
    sid: i8,
    pub srv_type: ServiceType,
    hu_tx: Sender<Packet>,
}

impl AAService {
    pub fn new(srv_type: ServiceType, sid:i8, tx: Sender<Packet>) -> Self {
        Self {
            sid,
            srv_type,
            hu_tx: tx,
        }
    }

    pub fn sid(&self) -> i8 {
        self.sid
    }

    pub fn enqueue_message(&self, msg: Packet) -> Result<()> {
        match self.hu_tx.try_send(msg) {
            Ok(()) => Ok(()),

            Err(mpsc::error::TrySendError::Full(_)) => {
                info!(
                "{:?}: Queue full, failed to enqueue message for sid {}",
                self.srv_type,
                self.sid
            );
                Ok(())
            }

            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(format!("{:?}: Queue closed for sid {}",
                    self.srv_type,
                    self.sid
                ).into())
            }
        }
    }

}

pub struct SrvSensorSource {
    pub base: AAService,
    rx: Receiver<Packet>,
    hu_tx: Sender<Packet>,
    sensors: Vec<SensorType>,
    prev_nt_mode:bool,
}

pub struct SrvMediaSinkVideoStreaming {
    pub base: AAService,
    rx: Receiver<Packet>,
    hu_tx: Sender<Packet>,
    adb_start_server:Arc<Notify>,
    projection_state:ProjectionStatus,
    video_params:VideoStreamingParams,
    cancel:CancellationToken,
    ignore_ack:bool,
    enabled:bool,
	//private members
    video_focus:bool,
    config_recived:bool,
    session_id:i32,
    video_streaming_started:bool,
    scrcpy_server:Option<VideoServerState>,
}

pub struct SrvMediaSinkAudioGuidance {
    pub base: AAService,
    rx: Receiver<Packet>,
    hu_tx: Sender<Packet>,
    acfg:AudioConfig,
    enabled:bool,
    audio_streaming_started:bool,
}

pub struct SrvMediaSinkAudioStreaming {
    pub base: AAService,
    rx: Receiver<Packet>,
    hu_tx: Sender<Packet>,
    adb_start_server:Arc<Notify>,
    acfg:AudioConfig,
    audio_params:AudioStreamingParams,
	cancel:CancellationToken,
    ignore_ack:bool,
    enabled:bool,
	//private members
    audio_stream_paused:bool,
    audio_focus:bool,
    config_recived:bool,
    session_id:i32,
	audio_streaming_started:bool,
    scrcpy_server:Option<AudioServerState>,
}

pub struct SrvMediaSource {
    pub base: AAService,
    rx: Receiver<Packet>,
    hu_tx: Sender<Packet>,
}
pub struct SrvInputSource {
    pub base: AAService,
    rx: Receiver<Packet>,
    hu_tx: Sender<Packet>,
    adb_start_server:Arc<Notify>,
    keys:Vec<i32>,
    cfg_screen_off:bool,
    cancel:CancellationToken,
    //private
    scrcpy_server:Option<ControlServerState>,
}

pub struct SrvVendorExtension {
    pub base: AAService,
    rx: Receiver<Packet>,
    hu_tx: Sender<Packet>,
}

pub struct SrvBluetooth {
    pub base: AAService,
    rx: Receiver<Packet>,
    hu_tx: Sender<Packet>,
}
//Service manager/control
pub struct ServiceManager {
    srv_type: ServiceType,
    hu_rx: Receiver<Packet>,
    hu_tx: Sender<Packet>,
    start_adb_server:Arc<Notify>,
    config: AppConfig,
    cancel:CancellationToken,
    //private fields
    audio_server_ready:Arc<Notify>,
    video_server_ready:Arc<Notify>,
    control_server_ready:Arc<Notify>,
    ch_opened:bool,
    sdr_services: Vec<Option<AAService>>,
    srv_tsk_handles:Vec<JoinHandle<Result<()>>>,
    sdr_sensors:Vec<SensorType>,
    sdr_keys: Vec<i32>,
    sdr_audio_cfg_guidance:AudioConfig,
    sdr_audio_cfg_streaming:AudioConfig,
    sdr_video_codec_params : VideoStreamingParams,
    sdr_audio_codec_params : AudioStreamingParams,
    sdr_control_server_sid: u8,
}
impl SrvSensorSource {
    pub fn new(sid:i8, hu_tx: Sender<Packet>, sensors: Vec<SensorType>) -> Self {
        let (tx, rx) = mpsc::channel(5);
        Self {
            base: AAService {
                sid,
                srv_type: ServiceType::SensorSource,
                hu_tx: tx,
            },
            rx,
            hu_tx,
            sensors,
            prev_nt_mode: false,
        }
    }

    pub fn start(self,cancel: CancellationToken,) -> (AAService, JoinHandle<Result<()>>) {
        let handle = self.base.clone();
        let task =tokio::spawn(async move {
            let mut service = self;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("{:?}: Stopping...",service.base.srv_type);
                        break;
                    }

                    msg = service.rx.recv() => {
                        match msg {
                            Some(msg) => {
                                service.handle_message( msg).await?;
                            }

                            None => {
                                // All Senders dropped
                                info!("{:?}: Channel closed",service.base.srv_type);
                                break;
                            }
                        }
                    }
                }
            }

            Ok(())
        });
        (handle, task)
    }

    async fn handle_message(&mut self, pkt: Packet) -> Result<()> {

        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
        {
            info!("{:?} Received message id: {}", self.base.srv_type, message_id);
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{:?}, channel {:?}: Wrong message status received", self.base.srv_type, pkt.channel);
                }
                else
                {
                    if self.sensors.contains(&SensorType::SENSOR_NIGHT_MODE) {
                        info!("{:?} send SENSOR_MESSAGE_REQUEST",self.base.srv_type);
                        let mut req = SensorRequest::new();
                        req.set_type(protos::SensorType::SENSOR_NIGHT_MODE);
                        req.set_min_update_period(1_000_000_000);
                        let mut payload: Vec<u8>=Vec::new();
                        payload.extend_from_slice(&(SensorMessageId::SENSOR_MESSAGE_REQUEST as u16).to_be_bytes());
                        payload.extend_from_slice(&(req.write_to_bytes()?));

                        let pkt_rsp = Packet {
                            channel: self.base.sid() as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        if let Err(_) = self.hu_tx.send(pkt_rsp).await
                        {
                            error!( "{:?} mpsc send error", self.base.srv_type);
                        };
                    }

                }
            }
            else {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == SENSOR_MESSAGE_RESPONSE  as i32
        {
            info!("{:?} Received message SENSOR_MESSAGE_RESPONSE", self.base.srv_type);
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = SensorResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{:?}, channel {:?}: Wrong message status received", self.base.srv_type, pkt.channel);
                }
            }
        }
        else if message_id == SENSOR_MESSAGE_BATCH  as i32
        {
            info!("{:?} Received message SENSOR_MESSAGE_BATCH", self.base.srv_type);
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = SensorBatch::parse_from_bytes(&data) {
                if !rsp.night_mode_data.is_empty()
                {
                    if let Some(night) = rsp.night_mode_data.first() {
                        let value = night.night_mode.unwrap_or(false);
                        if value != self.prev_nt_mode
                        {
                            self.prev_nt_mode=value;
                            info!("{:?} Switching theme for MD, night: {}", self.base.srv_type, value);
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
                            info!("{:?} ADB cmd shell response: {:?}",self.base.srv_type, line);
                            if !line.contains("Night mode:") && shell.id().is_some()
                            {
                                error!( "{:?} error switching MD theme", self.base.srv_type);
                            }
                            shell.kill().await?;
                        }
                    }
                }
            }
            else {
                error!( "{:?} error deserializing SensorBatch", self.base.srv_type);
            }
        }
        else if message_id == MESSAGE_CUSTOM_CMD  as i32
        {
            info!("{} Received {} message", self.base.sid.to_string(), message_id);
            let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
            if cmd == CustomCommand::CMD_OPEN_CH as i32
            {
                let mut open_req = ChannelOpenRequest::new();
                open_req.set_priority(0);
                open_req.set_service_id(self.base.sid as i32);
                let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: self.base.sid as u8,
                    flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                //tx_srv.send(pkt_rsp).await.expect("TODO: panic message");
                if let Err(_) = self.hu_tx.send(pkt_rsp).await
                {
                    error!( "{:?} mpsc send error", self.base.srv_type);
                };
            }
            else if cmd == CustomCommand::MD_DISCONNECTED as i32
            {
                info!( "{:?} MD_DISCONNECTED received", self.base.srv_type);
            }
        }
        else
        {
            info!( "{:?} Unknown message ID: {} received", self.base.srv_type, message_id);
        }

        Ok(())
    }
}
impl SrvMediaSinkVideoStreaming {
    pub fn new(sid:i8, hu_tx: Sender<Packet>, start_adb_server:Arc<Notify>, video_params:VideoStreamingParams, cancel:CancellationToken, ignore_ack:bool, enabled:bool) -> Self {
        let (tx, rx) = mpsc::channel(5);
        Self {
            base: AAService {
                sid,
                srv_type: ServiceType::MediaSink,
                hu_tx: tx,
            },
            rx,
            hu_tx:hu_tx.clone(),
            adb_start_server: start_adb_server,
            video_params:video_params.clone(),
            cancel: cancel.clone(),
            ignore_ack,
            enabled,
            projection_state:ProjectionStatus::TransitionToProjected,
            video_focus:false,
            config_recived:false,
            session_id:0,
            video_streaming_started:false,
            scrcpy_server: Some(VideoServerState::Created(
                crate::scrcpy::VideoServer::new(sid as u8, hu_tx.clone(), cancel.clone())
            )),
        }
    }

    pub fn start(self) -> (AAService, JoinHandle<Result<()>>) {
        let handle = self.base.clone();
        let task =tokio::spawn(async move {
            let mut service = self;
            loop {
                tokio::select! {
                    _ = service.cancel.cancelled() => {
                        info!("{:?}: Stopping...",service.base.srv_type);
                        break;
                    }

                    msg = service.rx.recv() => {
                        match msg {
                            Some(msg) => {
                                service.handle_message(msg).await?;
                            }

                            None => {
                                // All Senders dropped
                                info!("{:?}: Channel closed",service.base.srv_type);
                                break;
                            }
                        }
                    }
                }
            }

            Ok(())
        });
        (handle, task)
    }

    async fn handle_message(&mut self, pkt: Packet) -> Result<()> {

        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        info!("{:?} Received {} message", self.base.srv_type, message_id);
        if message_id == ControlMessageType::MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != MessageStatus::STATUS_SUCCESS
                {
                    error!( "{:?}, channel {:?}: Wrong message status received", self.base.srv_type, pkt.channel);
                }
                else {
                    if self.enabled
                    {
                        self.video_setup().await?;
                    }
                }
            }
            else {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == ControlMessageType::MESSAGE_CUSTOM_CMD  as i32
        {
            let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
            if cmd == CustomCommand::CMD_OPEN_CH as i32
            {
                let mut open_req = ChannelOpenRequest::new();
                open_req.set_priority(0);
                open_req.set_service_id(self.base.sid as i32);
                let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                payload.insert(0, ((ControlMessageType::MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                payload.insert(1, ((ControlMessageType::MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: self.base.sid as u8,
                    flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                    error!( "{:?} response send error",self.base.srv_type);
                };
            }
            else if cmd == CustomCommand::MD_DISCONNECTED as i32 {
                info!("{:?} MD disconnected, send media STOP to HU", self.base.srv_type);
                self.stop_media().await?;
                self.video_setup().await?;
            }
            else if cmd == CustomCommand::CMD_START_VIDEO_RECORDING as i32 {
                info!("{:?} CMD_START_VIDEO_RECORDING received, starting SCRCPY server", self.base.srv_type);
                if let Some(VideoServerState::Created(server)) = self.scrcpy_server.take() {
                    let handle = server.start(self.video_params.max_unack as u8);
                    self.scrcpy_server = Some(VideoServerState::Running(handle));
                } else {
                    error!("scrcpy_server: expected Created state, already started or missing");
                    self.cancel.cancel();
                }
            }
        }
        else if message_id == MediaMessageId::MEDIA_MESSAGE_CONFIG  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChConfig::parse_from_bytes(&data)
            {
                info!( "{:?}, channel {:?} MEDIA_MESSAGE_CONFIG received: Message status: {:?}, max_unack: {}", self.base.srv_type, pkt.channel, rsp.status(), rsp.max_unacked());
                if rsp.status() == ConfigStatus::STATUS_READY
                {
                    self.config_recived=true;
                    if self.ignore_ack
                    {
                        self.video_params.max_unack=0;
                    }
                    else
                    {
                        self.video_params.max_unack=rsp.max_unacked();
                    }

                }
            }
            else
            {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == MediaMessageId::MEDIA_MESSAGE_VIDEO_FOCUS_NOTIFICATION  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = VideoFocusNotification::parse_from_bytes(&data)
            {
                info!( "{:?}, channel {:?}: Message status: {:?}", self.base.srv_type, pkt.channel, rsp.focus());
                if (rsp.focus() == VideoFocusMode::VIDEO_FOCUS_PROJECTED) || (rsp.focus()==VideoFocusMode::VIDEO_FOCUS_PROJECTED_NO_INPUT_FOCUS)
                {
                    info!( "{:?}, channel {:?}: VIDEO_FOCUS_PROJECTED received", self.base.srv_type, pkt.channel);
                    self.video_focus=true;

                    if self.projection_state==ProjectionStatus::TransitionToProjected
                    {
                        self.session_id +=1;
                        self.start_media().await?;
                        self.start_scrcpy_media().await?;
                        self.projection_state=ProjectionStatus::ProjectedRecording;
                    }
                    else if self.projection_state==ProjectionStatus::ProjectedPause
                    {
                        self.resume_scrcpy_media().await?;
                        self.session_id +=1;
                        self.start_media().await?;
                        self.projection_state=ProjectionStatus::ProjectedRecording;
                    }
                    else
                    {
                        debug!("{:?}, channel {:?}: video streaming already started, ignoring packet", self.base.srv_type, pkt.channel);
                    }
                }
                else
                {
                    self.video_focus=false;
                    debug!( "{:?} video focus lost",self.base.srv_type);
                    if self.projection_state==ProjectionStatus::ProjectedRecording
                    {
                        self.pause_scrcpy_media().await?;
                        self.stop_media().await?;
                        self.projection_state=ProjectionStatus::ProjectedPause;
                    }
                }
            }
            else
            {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == MediaMessageId::MEDIA_MESSAGE_START  as i32//HU send this response as confirmation to START from MD, but only if STOP was sent before START
        {
            info!( "{:?}, channel {:?}: MEDIA_MESSAGE_START received", self.base.srv_type, pkt.channel);
        }
        else if message_id == MediaMessageId::MEDIA_MESSAGE_STOP  as i32
        {
            error!( "{:?}, channel {:?}: MEDIA_MESSAGE_STOP received but not managed", self.base.srv_type, pkt.channel);

        }
        else if message_id == MediaMessageId::MEDIA_MESSAGE_ACK  as i32
        {
            //error!("{:?}: Media ACK received by service, was not handled by PacketProxy", self.base.srv_type)
            if self.video_streaming_started
            {
                if let Some(VideoServerState::Running(server)) = &mut self.scrcpy_server {
                    server.ack();
                }
                else {
                    error!("{:?}: Unable to ack, scrcpy_server is None ", self.base.srv_type);
                }
            }
        }
        else
        {
            info!( "{:?} Unknown message ID: {} received", self.base.srv_type, message_id);
        }
        Ok(())
    }

    async fn video_setup(&self)->Result<()> {
        info!( "{:?}, channel {:?}: Sending SETUP command", self.base.srv_type, self.base.sid);
        let mut media_setup= Setup::new();
        media_setup.set_type(MediaCodecType::MEDIA_CODEC_VIDEO_H264_BP);
        let mut payload: Vec<u8>=Vec::new();
        payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_SETUP as u16).to_be_bytes());
        payload.extend_from_slice(&(media_setup.write_to_bytes()?));
        let pkt_rsp = Packet {
            channel: self.base.sid as u8,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = self.hu_tx.send(pkt_rsp).await{
            error!( "{:?} send error",self.base.srv_type);
        };
        Ok(())
    }
    async fn start_media(&mut self) ->Result<()> {
        info!( "{:?}, channel {:?}: Sending START command, session id= {}", self.base.srv_type, self.base.sid, self.session_id);
        let mut start_req = Start::new();
        start_req.set_session_id(self.session_id);
        start_req.set_configuration_index(0);
        let mut payload: Vec<u8> = start_req.write_to_bytes().expect("serialization failed");
        payload.insert(0, ((MediaMessageId::MEDIA_MESSAGE_START as u16) >> 8) as u8);
        payload.insert(1, ((MediaMessageId::MEDIA_MESSAGE_START as u16) & 0xff) as u8);

        let pkt_rsp = Packet {
            channel: self.base.sid as u8,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = self.hu_tx.send(pkt_rsp).await{
            error!( "{:?} response send error",self.base.srv_type);
        };
        self.video_streaming_started=true;
        Ok(())
    }
    async fn stop_media(&mut self) ->Result<()> {
        info!( "{:?}, channel {:?}: Sending STOP command", self.base.srv_type, self.base.sid);
        let media_stop= Stop::new();
        let mut payload: Vec<u8>=Vec::new();
        payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_STOP as u16).to_be_bytes());
        payload.extend_from_slice(&(media_stop.write_to_bytes()?));
        let pkt_rsp = Packet {
            channel: self.base.sid as u8,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = self.hu_tx.send(pkt_rsp).await{
            error!( "{:?} send error", self.base.srv_type);
        };
        self.video_streaming_started=false;
        Ok(())
    }
    async fn start_scrcpy_media(&mut self) ->Result<()> {
        debug!( "{:?}, Notify video streaming ready", self.base.srv_type);
        self.adb_start_server.notify_one();
        Ok(())
    }
    async fn pause_scrcpy_media(&self)->Result<()> {
        debug!( "{:?}, Pausing video streaming", self.base.srv_type);
        if let Some(VideoServerState::Running(server)) = &self.scrcpy_server {
            server.set_paused(true);
        }
        Ok(())
    }
    async fn resume_scrcpy_media(&self)->Result<()> {
        debug!( "{:?}, Resuming video streaming", self.base.srv_type);
        if let Some(VideoServerState::Running(server)) = &self.scrcpy_server {
            server.set_paused(false);
        }
        Ok(())
    }
}
impl SrvMediaSinkAudioStreaming {
    pub fn new(sid:i8, hu_tx: Sender<Packet>, start_adb_server:Arc<Notify> ,acfg:AudioConfig,audio_params:AudioStreamingParams, cancel:CancellationToken, ignore_ack:bool, enabled:bool) -> Self {
        let (tx, rx) = mpsc::channel(5);
        Self {
            base: AAService {
                sid,
                srv_type: ServiceType::MediaSink,
                hu_tx: tx,
            },
            rx,
            hu_tx:hu_tx.clone(),
            adb_start_server:start_adb_server,
            acfg,
            audio_params:audio_params.clone(),
			cancel: cancel.clone(),
            ignore_ack,
            enabled,
            audio_streaming_started :false,
            audio_stream_paused:false,
            audio_focus:false,
            config_recived:false,
            session_id:1,
            scrcpy_server: Some(AudioServerState::Created(
                crate::scrcpy::AudioServer::new(sid as u8,hu_tx.clone(), cancel.clone()))
            ),
        }
    }

    pub fn start(self) -> (AAService, JoinHandle<Result<()>>) {
        let handle = self.base.clone();
        let task =tokio::spawn(async move {
            let mut service = self;
            loop {
                tokio::select! {
                    _ = service.cancel.cancelled() => {
                        info!("{:?}: Stopping...",service.base.srv_type);
                        break;
                    }

                    msg = service.rx.recv() => {
                        match msg {
                            Some(msg) => {
                                service.handle_message( msg).await?;
                            }

                            None => {
                                // All Senders dropped
                                info!("{:?}: Channel closed",service.base.srv_type);
                                break;
                            }
                        }
                    }
                }
            }

            Ok(())
        });
        (handle, task)
    }

    async fn handle_message(&mut self, pkt: Packet) -> Result<()> {

        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        info!("{:?} Received message id {}", self.base.srv_type, message_id);
        if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{:?}, channel {:?}: Wrong message status received", self.base.srv_type, pkt.channel);
                }
                else {
                    self.audio_focus=true;
                    if self.enabled
                    {
                        let mut cfg_req= Setup::new();
                        if self.acfg.codec == MediaCodec::AUDIO_PCM
                        {
                            cfg_req.set_type(MediaCodecType::MEDIA_CODEC_AUDIO_PCM);
                        }
                        else if self.acfg.codec == MediaCodec::AUDIO_AAC_LC
                        {
                            cfg_req.set_type(MediaCodecType::MEDIA_CODEC_AUDIO_AAC_LC);
                        }
                        else {
                            error!("{:?}: Unsupported audio codec detected", self.base.srv_type)
                        }

                        let mut payload: Vec<u8>=cfg_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) >> 8) as u8);
                        payload.insert( 1,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: self.base.sid as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                            error!( "{:?} mpsc send error",self.base.srv_type);
                        };
                    }
                }
            }
            else {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == MESSAGE_CUSTOM_CMD  as i32
        {
            let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
            if cmd == CustomCommand::CMD_OPEN_CH as i32
            {
                let mut open_req = ChannelOpenRequest::new();
                open_req.set_priority(0);
                open_req.set_service_id(self.base.sid as i32);
                let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: self.base.sid as u8,
                    flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                    error!( "{:?} response send error",self.base.srv_type);
                };
            }
            else if cmd == CustomCommand::MD_DISCONNECTED as i32 {
                debug!("{:?} MD diconnected",self.base.srv_type);
                self.stop_media().await?;
            }
            else if cmd == CustomCommand::CMD_START_AUDIO_RECORDING as i32 {
                debug!("{:?} CMD_START_AUDIO_RECORDING received, starting SCRCPY server",self.base.srv_type);
                if let Some(AudioServerState::Created(server)) = self.scrcpy_server.take() {
                    let handle = server.start(self.audio_params.max_unack as u8);
                    self.scrcpy_server = Some(AudioServerState::Running(handle));
                } else {
                    error!("{:?} scrcpy_server: expected Created state, already started or missing", self.base.srv_type);
                    self.cancel.cancel();
                }
            }
        }
        else if message_id == MEDIA_MESSAGE_CONFIG  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChConfig::parse_from_bytes(&data)
            {
                info!( "{:?}, channel {:?} MEDIA_MESSAGE_CONFIG received: Message status: {:?}, max_unack: {}", self.base.srv_type, pkt.channel, rsp.status(), rsp.max_unacked());
                if rsp.status() == STATUS_READY
                {
                    self.config_recived=true;
                    if self.ignore_ack
                    {
                        self.audio_params.max_unack=0;
                    }
                    else
                    {
                        self.audio_params.max_unack=rsp.max_unacked();
                    }

                    //info!( "{}, channel {:?}: Starting audio capture", get_name(), pkt.channel);
                    if (self.acfg.codec == MediaCodec::AUDIO_PCM) || (self.acfg.codec == MediaCodec::AUDIO_AAC_LC)
                    {
                        self.session_id +=1;
                        self.start_media().await?;
                        self.audio_streaming_started =true;
                        info!( "{:?} Notify audio streaming ready",self.base.srv_type);
						self.adb_start_server.notify_one();
                    }
                    else
                    {
                        error!( "{:?}: Unsupported audio codec detected", self.base.srv_type);
                    }
                }
            }
            else
            {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == MediaMessageId::MEDIA_MESSAGE_ACK  as i32 //now this is done by PacketProxy, not needed
        {
            //error!("{:?}: Media ACK received by service, was not handled by PacketProxy", self.base.srv_type)
            if self.audio_streaming_started
            {
                if let Some(AudioServerState::Running(server)) = &mut self.scrcpy_server {
                    server.ack();
                }
            }
        }
        else if message_id == MediaMessageId::MEDIA_MESSAGE_AUDIO_UNDERFLOW_NOTIFICATION  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(_) = AudioUnderflowNotification::parse_from_bytes(&data)
            {
                debug!("{:?} Received MEDIA_MESSAGE_AUDIO_UNDERFLOW_NOTIFICATION", self.base.srv_type);
            }
            else {
                error!("{:?}: Unable to deserialize AudioUnderflowNotification", self.base.srv_type)
            }
        }
        else if message_id == ControlMessageType::MESSAGE_AUDIO_FOCUS_NOTIFICATION as i32
        {
            //Proxy msg from Control channel
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if let Ok(msg) = AudioFocusNotification::parse_from_bytes(&data) {
                debug!("{} Received MESSAGE_AUDIO_FOCUS_NOTIFICATION", self.base.srv_type);
                if (msg.focus_state() == AudioFocusStateType::AUDIO_FOCUS_STATE_GAIN) || (msg.focus_state() == AudioFocusStateType::AUDIO_FOCUS_STATE_GAIN_TRANSIENT) || (msg.focus_state() == AudioFocusStateType::AUDIO_FOCUS_STATE_GAIN_MEDIA_ONLY)
                {
                    self.audio_focus=true;
                    if self.audio_streaming_started
                    {
                        if self.audio_stream_paused
                        {
                            debug!("{:?}: Resuming audio stream", self.base.srv_type);
                            self.audio_stream_paused=false;
                            self.start_media().await?;
                            if let Some(AudioServerState::Running(server)) = &self.scrcpy_server {
                                server.set_paused(false);
                            }
                            else {
                                error!("{:?}: Unable to resume audio stream, scrcpy_server is None ", self.base.srv_type);
                            }
                        }
                    }
                    else {
                        error!("{:?}: Audio stream not started, ignoring message", self.base.srv_type);
                    }
                }
                else {
                    //focus lost
                    self.audio_focus=false;
                    if self.audio_streaming_started
                    {
                        if !self.audio_stream_paused
                        {
                            debug!("{:?}: Pausing audio stream", self.base.srv_type);
                            self.audio_stream_paused=true;
                            self.stop_media().await?;
                            if let Some(AudioServerState::Running(server)) = &self.scrcpy_server {
                                server.set_paused(true);
                            }
                            else {
                                error!("{:?}: Unable to pause audio stream, scrcpy_server is None ", self.base.srv_type);
                            }

                        }
                    }
                }
            }
            else {
                error!("{}: Unable to deserialize AudioFocusNotification", self.base.sid.to_string())
            }
        }
        else
        {
            error!( "{:?} Unknown message ID: {} received", self.base.srv_type, message_id);
        }

        Ok(())
    }

    async fn stop_media(&self)->Result<()> {
        info!( "{:?}, channel {:?}: Sending STOP command", self.base.srv_type, self.base.sid);
        let media_stop= Stop::new();
        let mut payload: Vec<u8>=Vec::new();
        payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_STOP as u16).to_be_bytes());
        payload.extend_from_slice(&(media_stop.write_to_bytes()?));
        let pkt_rsp = Packet {
            channel: self.base.sid as u8,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = self.hu_tx.send(pkt_rsp).await{
            error!( "{:?} send error",self.base.srv_type);
        };
        Ok(())
    }
    async fn start_media(&self)->Result<()> {
        info!( "{:?}, channel {:?}: Sending START command", self.base.srv_type, self.base.sid);
        let mut start_req = Start::new();
        start_req.set_session_id(self.session_id);
        start_req.set_configuration_index(0);
        let mut payload: Vec<u8> = start_req.write_to_bytes().expect("serialization failed");
        payload.insert(0, ((MediaMessageId::MEDIA_MESSAGE_START as u16) >> 8) as u8);
        payload.insert(1, ((MediaMessageId::MEDIA_MESSAGE_START as u16) & 0xff) as u8);

        let pkt_rsp = Packet {
            channel: self.base.sid as u8,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = self.hu_tx.send(pkt_rsp).await{
            error!( "{:?} response send error",self.base.srv_type);
        };
        Ok(())
    }
}
impl SrvMediaSinkAudioGuidance {
    pub fn new(sid:i8, hu_tx: Sender<Packet>, acfg:AudioConfig, enabled:bool) -> Self {
        let (tx, rx) = mpsc::channel(5);
        Self {
            base: AAService {
                sid,
                srv_type: ServiceType::SensorSource,
                hu_tx: tx,
            },
            rx,
            hu_tx,
            acfg,
            enabled,
            audio_streaming_started: false,
        }
    }

    pub fn start(self,cancel: CancellationToken,) -> (AAService, JoinHandle<Result<()>>) {
        let handle = self.base.clone();
        let task =tokio::spawn(async move {
            let mut service = self;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("{:?}: Stopping...",service.base.srv_type);
                        break;
                    }

                    msg = service.rx.recv() => {
                        match msg {
                            Some(msg) => {
                                service.handle_message( msg).await?;
                            }

                            None => {
                                // All Senders dropped
                                info!("{:?}: Channel closed",service.base.srv_type);
                                break;
                            }
                        }
                    }
                }
            }

            Ok(())
        });
        (handle, task)
    }

    async fn handle_message(&mut self, pkt: Packet) -> Result<()> {

        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        info!("{:?} Received message id {}", self.base.srv_type, message_id);
        if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
        {

            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{:?}, channel {:?}: Wrong message status received", self.base.srv_type, pkt.channel);
                }
                else
                {
                    if self.enabled
                    {
                        let mut cfg_req= Setup::new();
                        if self.acfg.codec == MediaCodec::AUDIO_PCM
                        {
                            cfg_req.set_type(MediaCodecType::MEDIA_CODEC_AUDIO_PCM);
                        }
                        else if self.acfg.codec == MediaCodec::AUDIO_AAC_LC
                        {
                            cfg_req.set_type(MediaCodecType::MEDIA_CODEC_AUDIO_AAC_LC);
                        }
                        else {
                            error!("{:?}: Unsupported audio codec detected", self.base.srv_type)
                        }

                        let mut payload: Vec<u8>=cfg_req.write_to_bytes().expect("serialization failed");
                        payload.insert(0,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) >> 8) as u8);
                        payload.insert( 1,((MediaMessageId::MEDIA_MESSAGE_SETUP as u16) & 0xff) as u8);

                        let pkt_rsp = Packet {
                            channel: self.base.sid as u8,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                            error!( "{:?} mpsc send error",self.base.srv_type);
                        };
                    }
                }
            }
            else {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == MESSAGE_CUSTOM_CMD  as i32
        {
            let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
            if cmd == CustomCommand::CMD_OPEN_CH as i32
            {
                let mut open_req = ChannelOpenRequest::new();
                open_req.set_priority(0);
                open_req.set_service_id(self.base.sid as i32);
                let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: self.base.sid as u8,
                    flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = self.hu_tx.send(pkt_rsp).await {
                    error!( "{:?} response send error",self.base.srv_type);
                };
            }
            else if (cmd == CustomCommand::CMD_SETUP_CH as i32) && self.enabled
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
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChConfig::parse_from_bytes(&data)
            {
                info!( "{:?}, channel {:?}: Message status: {:?}", self.base.srv_type, pkt.channel, rsp.status());
                if rsp.status() == STATUS_READY
                {
                    info!( "{:?}, channel {:?}: Starting audio capture", self.base.srv_type, pkt.channel);
                    if self.acfg.codec == MediaCodec::AUDIO_PCM
                    {
                        self.audio_streaming_started =true;
                    }
                    else
                    {
                        error!( "{:?}: Unsupported audio codec detected", self.base.srv_type);
                    }
                }
            }
            else
            {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == MediaMessageId::MEDIA_MESSAGE_ACK  as i32
        {
            //info!("{:?} Received MEDIA_MESSAGE_ACK", self.base.srv_type);
        }
        else
        {
            info!( "{:?} Unknown message ID: {} received", self.base.srv_type, message_id);
        }

        Ok(())
    }
}
impl SrvMediaSource {
    pub fn new(sid:i8, hu_tx: Sender<Packet>) -> Self {
        let (tx, rx) = mpsc::channel(5);
        Self {
            base: AAService {
                sid,
                srv_type: ServiceType::MediaSource,
                hu_tx: tx,
            },
            rx,
            hu_tx,
        }
    }

    pub fn start(self,cancel: CancellationToken,) -> (AAService, JoinHandle<Result<()>>) {
        let handle = self.base.clone();
        let task =tokio::spawn(async move {
            let mut service = self;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("{:?}: Stopping...",service.base.srv_type);
                        break;
                    }

                    msg = service.rx.recv() => {
                        match msg {
                            Some(msg) => {
                                service.handle_message( msg).await?;
                            }

                            None => {
                                // All Senders dropped
                                info!("{:?}: Channel closed",service.base.srv_type);
                                break;
                            }
                        }
                    }
                }
            }

            Ok(())
        });
        (handle, task)
    }

    async fn handle_message(&mut self, pkt: Packet) -> Result<()> {

        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        info!("{:?} Received message id {}", self.base.srv_type, message_id);
        if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{:?}, channel {:?}: Wrong message status received", self.base.srv_type, pkt.channel);
                }
            }
            else {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == MESSAGE_CUSTOM_CMD  as i32
        {
            let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
            if cmd == CustomCommand::CMD_OPEN_CH as i32
            {
                let mut open_req = ChannelOpenRequest::new();
                open_req.set_priority(0);
                open_req.set_service_id(self.base.sid as i32);
                let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: self.base.sid as u8,
                    flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                    error!( "{:?} response send error",self.base.srv_type);
                };
            }
        }
        else {
            info!( "{:?} Unknown message ID: {} received", self.base.srv_type, message_id);
        }

        Ok(())
    }
}
impl SrvInputSource {
    pub fn new(sid:i8, hu_tx: Sender<Packet>, start_adb_server:Arc<Notify>, keys:Vec<i32>,screen_size:ScrcpySize,cfg_screen_off:bool, cancel: CancellationToken) -> Self {
        let (tx, rx) = mpsc::channel(5);
        Self {
            base: AAService {
                sid,
                srv_type: ServiceType::InputSource,
                hu_tx: tx,
            },
            rx,
            hu_tx:hu_tx.clone(),
            adb_start_server: start_adb_server,
            keys,
            cfg_screen_off,
            cancel:cancel.clone(),
            scrcpy_server: Some(ControlServerState::Created(
                crate::scrcpy::ControlServer::new(sid as u8,hu_tx.clone(), screen_size, cfg_screen_off,cancel.clone()))
            ),
        }
    }

    pub fn start(self,) -> (AAService, JoinHandle<Result<()>>) {
        let handle = self.base.clone();
        let task =tokio::spawn(async move {
            let mut service = self;
            loop {
                tokio::select! {
                    _ = service.cancel.cancelled() => {
                        info!("{:?}: Stopping...",service.base.srv_type);
                        break;
                    }

                    msg = service.rx.recv() => {
                        match msg {
                            Some(msg) => {
                                service.handle_message( msg).await?;
                            }

                            None => {
                                // All Senders dropped
                                info!("{:?}: Channel closed",service.base.srv_type);
                                break;
                            }
                        }
                    }
                }
            }

            Ok(())
        });
        (handle, task)
    }

    async fn handle_message(&mut self, pkt: Packet) -> Result<()> {

        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        info!("{:?} Received message id {}", self.base.srv_type, message_id);
        if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{:?}, channel {:?}: Wrong message status received", self.base.srv_type, pkt.channel);
                }
                else {
                    //FIXME send BindingRequest
                    let mut binding_req = KeyBindingRequest::new();
                    binding_req.keycodes.extend_from_slice(&self.keys);

                    let mut payload: Vec<u8>=Vec::new();
                    payload.extend_from_slice(&(InputMessageId::INPUT_MESSAGE_KEY_BINDING_REQUEST as u16).to_be_bytes());
                    payload.extend_from_slice(&binding_req.write_to_bytes().expect("serialization failed"));

                    let pkt_rsp = Packet {
                        channel: self.base.sid as u8,
                        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                        error!( "{:?} response send error",self.base.srv_type);
                    };
                }
            }
            else {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == MESSAGE_CUSTOM_CMD  as i32
        {
            let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
            if cmd == CustomCommand::CMD_OPEN_CH as i32
            {
                let mut open_req = ChannelOpenRequest::new();
                open_req.set_priority(0);
                open_req.set_service_id(self.base.sid as i32);
                let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: self.base.sid as u8,
                    flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                    error!( "{:?} response send error",self.base.srv_type);
                };
            }
            else if cmd == CustomCommand::CMD_START_CONTROL_SERVER as i32
            {
                if let Some(ControlServerState::Created(server)) = self.scrcpy_server.take() {
                    self.scrcpy_server = Some(ControlServerState::Running(server.start()));
                }
                else {
                    error!( "{:?} Unable to start control server",self.base.srv_type);
                    self.cancel.cancel();
                }
            }
        }
        else if message_id == InputMessageId::INPUT_MESSAGE_INPUT_REPORT  as i32
        {
            if let Some(ControlServerState::Running(server)) = &self.scrcpy_server {
                server.enque_msg(pkt).await;
            }
            else {
                error!( "{:?} scrcpy_cmd send error",self.base.srv_type);
            }
        }
        else if message_id == InputMessageId::INPUT_MESSAGE_KEY_BINDING_RESPONSE  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = KeyBindingResponse::parse_from_bytes(&data) {
                debug!("{:?} Decoded KeyBindingResponse status: {:?}",self.base.srv_type, rsp.status());
                if let Some(ControlServerState::Created(server)) = self.scrcpy_server.take() {
                    debug!("{:?} Notify control server ready",self.base.srv_type);
                    self.adb_start_server.notify_one();
                }
            }
        }
        else {
            error!( "{:?} Unmanaged message ID: {} received", self.base.srv_type, message_id);
        }

        Ok(())
    }
}
impl SrvVendorExtension {
    pub fn new(sid:i8, hu_tx: Sender<Packet>) -> Self {
        let (tx, rx) = mpsc::channel(5);
        Self {
            base: AAService {
                sid,
                srv_type: ServiceType::VendorExtension,
                hu_tx: tx,
            },
            rx,
            hu_tx,
        }
    }

    pub fn start(self,cancel: CancellationToken,) -> (AAService, JoinHandle<Result<()>>) {
        let handle = self.base.clone();
        let task =tokio::spawn(async move {
            let mut service = self;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("{:?}: Stopping...",service.base.srv_type);
                        break;
                    }

                    msg = service.rx.recv() => {
                        match msg {
                            Some(msg) => {
                                service.handle_message( msg).await?;
                            }

                            None => {
                                // All Senders dropped
                                info!("{:?}: Channel closed",service.base.srv_type);
                                break;
                            }
                        }
                    }
                }
            }

            Ok(())
        });
        (handle, task)
    }

    async fn handle_message(&mut self, pkt: Packet) -> Result<()> {

        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        info!("{:?} Received message id {}", self.base.srv_type, message_id);
        if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE  as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if  let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{:?}, channel {:?}: Wrong message status received", self.base.srv_type, pkt.channel);
                }
            }
            else {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        }
        else if message_id == MESSAGE_CUSTOM_CMD  as i32
        {
            let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
            if cmd == CustomCommand::CMD_OPEN_CH as i32
            {
                let mut open_req = ChannelOpenRequest::new();
                open_req.set_priority(0);
                open_req.set_service_id(self.base.sid as i32);
                let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: self.base.sid as u8,
                    flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                    error!( "{:?} response send error",self.base.srv_type);
                };
            }
        }
        else {
            info!( "{:?} Unknown message ID: {} received", self.base.srv_type, message_id);
        }
        Ok(())
    }
}
impl SrvBluetooth {
    pub fn new(sid: i8, hu_tx: Sender<Packet>) -> Self {
        let (tx, rx) = mpsc::channel(5);
        Self {
            base: AAService {
                sid,
                srv_type: ServiceType::Bluetooth,
                hu_tx: tx,
            },
            rx,
            hu_tx,
        }
    }

    pub fn start(self, cancel: CancellationToken, ) -> (AAService, JoinHandle<Result<()>>) {
        let handle = self.base.clone();
        let task = tokio::spawn(async move {
            let mut service = self;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("{:?}: Stopping...",service.base.srv_type);
                        break;
                    }

                    msg = service.rx.recv() => {
                        match msg {
                            Some(msg) => {
                                service.handle_message( msg).await?;
                            }

                            None => {
                                // All Senders dropped
                                info!("{:?}: Channel closed",service.base.srv_type);
                                break;
                            }
                        }
                    }
                }
            }

            Ok(())
        });
        (handle, task)
    }

    async fn handle_message(&mut self, pkt: Packet) -> Result<()> {
        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
        info!("{:?} Received message id {}", self.base.srv_type, message_id);
        if message_id == MESSAGE_CHANNEL_OPEN_RESPONSE as i32
        {
            let data = &pkt.payload[2..]; // start of message data, without message_id
            if let Ok(rsp) = ChannelOpenResponse::parse_from_bytes(&data) {
                if rsp.status() != STATUS_SUCCESS
                {
                    error!( "{:?}, channel {:?}: Wrong message status received", self.base.srv_type, pkt.channel);
                }
            } else {
                error!( "{:?}, channel {:?}: Unable to parse received message", self.base.srv_type, pkt.channel);
            }
        } else if message_id == MESSAGE_CUSTOM_CMD as i32
        {
            let cmd: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
            if cmd == CustomCommand::CMD_OPEN_CH as i32
            {
                let mut open_req = ChannelOpenRequest::new();
                open_req.set_priority(0);
                open_req.set_service_id(self.base.sid as i32);
                let mut payload: Vec<u8> = open_req.write_to_bytes().expect("serialization failed");
                payload.insert(0, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
                payload.insert(1, ((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: self.base.sid as u8,
                    flags: ENCRYPTED | FRAME_TYPE_CONTROL | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = self.hu_tx.send(pkt_rsp).await {
                    error!( "{:?} response send error",self.base.srv_type);
                };
            }
        } else {
            info!( "{:?} Unknown message ID: {} received", self.base.srv_type, message_id);
        }
        Ok(())
    }
}

impl ServiceManager {
    pub fn new(hu_rx: Receiver<Packet>, hu_tx: Sender<Packet>, start_adb_server: Arc<Notify>, config: AppConfig, cancel:CancellationToken) -> Self {
        //This service is different, we don't own mspc channels, we use those passed by parameters
        Self {
            srv_type: ServiceType::Control,
            hu_rx,
            hu_tx,
            start_adb_server,
            config,
            cancel,
            ch_opened:false,
            audio_server_ready: Arc::new(Notify::new()),
            video_server_ready: Arc::new(Notify::new()),
            control_server_ready: Arc::new(Notify::new()),
            sdr_services: Vec::new(),
            srv_tsk_handles:Vec::new(),
            sdr_sensors:Vec::new(),
            sdr_keys:Vec::new(),
            sdr_audio_cfg_guidance: AudioConfig::default(),
            sdr_audio_cfg_streaming: AudioConfig::default(),
            sdr_video_codec_params : VideoStreamingParams::default(),
            sdr_audio_codec_params : AudioStreamingParams::default(),
            sdr_control_server_sid:0,
        }
    }
    pub fn start(self, cancel: CancellationToken, ) -> (JoinHandle<Result<()>>) {
        let task = tokio::spawn(async move {
            let mut service = self;
            info!( "{:?} Starting channel manager",service.srv_type);
            let mut audio_srv_ready=false;
            let mut video_srv_ready=false;
            let mut control_srv_ready=false;
            while !service.cancel.is_cancelled() {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("{:?}: Stopping...",service.srv_type);
                        break;
                    }
                    _ = service.audio_server_ready.notified() => {
                        // Notification received
                        audio_srv_ready=true;
                        if(audio_srv_ready && video_srv_ready && control_srv_ready)
                        {
                            service.start_adb_servers().await?;
                        }
                    }
                    _ = service.video_server_ready.notified() => {
                        // Notification received
                        video_srv_ready=true;
                        if(audio_srv_ready && video_srv_ready && control_srv_ready)
                        {
                            service.start_adb_servers().await?;
                        }
                    }
                    _ = service.control_server_ready.notified() => {
                        // Notification received
                        control_srv_ready=true;
                        if(audio_srv_ready && video_srv_ready && control_srv_ready)
                        {
                            service.start_adb_servers().await?;
                        }
                    }
                    msg = service.hu_rx.recv() => {
                        match msg {
                            Some(msg) => {
                                service.handle_hu_message( msg).await?;
                            }

                            None => {
                                // All Senders dropped
                                info!("{:?}: HU Channel closed",service.srv_type);
                                break;
                            }
                        }
                    }
                }
            }

            Ok(())
        });
        (task)
    }
    async fn handle_hu_message(&mut self, mut pkt: Packet) -> Result<()> {
        if pkt.channel == 0
        {
            //Control channel
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            info!("{:?} Received message id {}", self.srv_type, message_id);
            if message_id == ControlMessageType::MESSAGE_VERSION_REQUEST as i32
            {
                info!( "{:?} HU version request received, sending VersionResponse back...",self.srv_type);
                // build version response for HU
                //let mut response = VersionResponse::new();
                //let mut payload: Vec<u8> = response.write_to_bytes()?;
                let mut payload: Vec<u8>=Vec::new();
                payload.push(((ControlMessageType::MESSAGE_VERSION_RESPONSE as u16) >> 8) as u8);
                payload.push( ((ControlMessageType::MESSAGE_VERSION_RESPONSE as u16) & 0xff) as u8);
                payload.push( pkt.payload[2]);//send back same version as requested
                payload.push( pkt.payload[3]);
                payload.push( pkt.payload[4]);
                payload.push( pkt.payload[5]);
                payload.push( ((MessageStatus::STATUS_SUCCESS  as u16) >> 8) as u8);
                payload.push( ((MessageStatus::STATUS_SUCCESS  as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: 0,
                    flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                    error!( "{:?} tls proxy send error",self.srv_type);
                };
            }
            else if message_id == ControlMessageType::MESSAGE_AUTH_COMPLETE as i32
            {
                info!( "{:?} MESSAGE_AUTH_COMPLETE received",self.srv_type);
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = AuthResponse::parse_from_bytes(&data) {
                    if msg.status() != auth_response::Status::OK
                    {
                        error!( "{:?} AuthResponse status is not OK, got {:?}",self.srv_type, msg.status);
                        return Err(Box::new("AuthResponse status is not OK")).expect("AuthResponse.OK");
                    }
                }
                else {
                    error!( "{:?} AuthResponse couldn't be parsed",self.srv_type);
                    return Err(Box::new("AuthResponse couldn't be parsed")).expect("AuthResponse");
                }

                info!( "{:?} Sending ServiceDiscovery request...",self.srv_type);
                let icon32 = std::fs::read(format!("{}{}", crate::channel_manager::RES_PATH, "/AndroidIcon32.png"));
                let icon64 = std::fs::read(format!("{}{}", crate::channel_manager::RES_PATH, "/AndroidIcon64.png"));
                let icon128 = std::fs::read(format!("{}{}", crate::channel_manager::RES_PATH, "/AndroidIcon128.png"));
                let mut sdreq= ServiceDiscoveryRequest::new();
                sdreq.set_small_icon(icon32.unwrap());
                sdreq.set_medium_icon(icon64.unwrap());
                sdreq.set_large_icon(icon128.unwrap());
                sdreq.set_label_text("aa-mirror-rs".to_owned());
                sdreq.set_device_name("aa-mirror-os".to_owned());
                let mut payload: Vec<u8>=sdreq.write_to_bytes()?;
                payload.insert(0,((ControlMessageType::MESSAGE_SERVICE_DISCOVERY_REQUEST as u16) >> 8) as u8);
                payload.insert( 1,((ControlMessageType::MESSAGE_SERVICE_DISCOVERY_REQUEST as u16) & 0xff) as u8);

                let pkt_rsp = Packet {
                    channel: 0,
                    flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                    error!( "{} tls proxy send error",self.srv_type);
                };
            }
            else if message_id == ControlMessageType::MESSAGE_SERVICE_DISCOVERY_RESPONSE as i32
            {
                let _ = pkt_debug(
                    HexdumpLevel::Disabled,
                    HexdumpLevel::DecryptedInput,
                    &pkt,
                    "HU".parse().unwrap()
                ).await;

                let data = &pkt.payload[2..]; // start of message data, without message_id

                if  let Ok(msg) = ServiceDiscoveryResponse::parse_from_bytes(&data){
                    info!( "{:?} ServiceDiscoveryResponse parsed ok",self.srv_type);
                    for (_,proto_srv) in msg.services.iter().enumerate() {
                        let ch_id=i32::from(proto_srv.id());
                        if proto_srv.media_sink_service.is_some()
                        {
                            if proto_srv.media_sink_service.audio_configs.len()>0
                            {
                                let srv_type=proto_srv.media_sink_service.audio_type();
                                let acd=match proto_srv.media_sink_service.available_type() {
                                    MediaCodecType::MEDIA_CODEC_AUDIO_AAC_LC_ADTS=>AUDIO_AAC_LC_ADTS,
                                    MediaCodecType::MEDIA_CODEC_AUDIO_AAC_LC=>AUDIO_AAC_LC,
                                    MediaCodecType::MEDIA_CODEC_AUDIO_PCM=>AUDIO_PCM,
                                    _=>AUDIO_PCM,
                                };
                                if srv_type == AudioStreamType::AUDIO_STREAM_GUIDANCE
                                {
                                    self.sdr_audio_cfg_guidance =AudioConfig
                                    {
                                        codec:acd,
                                        stream_type: AudioStream::GUIDANCE,
                                        bitrate:proto_srv.media_sink_service.audio_configs[0].sampling_rate(),
                                        channels:proto_srv.media_sink_service.audio_configs[0].number_of_channels(),
                                        bitdepth:proto_srv.media_sink_service.audio_configs[0].number_of_bits(),
                                    };
                                    let service = SrvMediaSinkAudioGuidance::new(ch_id as i8, self.hu_tx.clone(), self.sdr_audio_cfg_guidance.clone(), false);
                                    let (service_handle, task) = service.start(self.cancel.clone());
                                    self.add_service(service_handle);
                                    self.srv_tsk_handles.push(task);
                                }
                                else if srv_type == AudioStreamType::AUDIO_STREAM_MEDIA
                                {
                                    self.sdr_audio_cfg_streaming =AudioConfig
                                    {
                                        codec:acd,
                                        stream_type: AudioStream::MEDIA,
                                        bitrate:proto_srv.media_sink_service.audio_configs[0].sampling_rate(),
                                        channels:proto_srv.media_sink_service.audio_configs[0].number_of_channels(),
                                        bitdepth:proto_srv.media_sink_service.audio_configs[0].number_of_bits(),
                                    };
                                    self.sdr_audio_codec_params.bitrate=self.sdr_audio_cfg_streaming.bitrate as i32;
                                    self.sdr_audio_codec_params.sid=ch_id as u8;
                                    self.sdr_audio_codec_params.codec=acd;

                                    let service = SrvMediaSinkAudioStreaming::new(ch_id as i8, self.hu_tx.clone(), self.audio_server_ready.clone(), self.sdr_audio_cfg_streaming.clone(), self.sdr_audio_codec_params.clone(), self.cancel.clone(), self.config.ignore_media_ack,true);
                                    let (service_handle, task) = service.start();
                                    self.add_service(service_handle);
                                    self.srv_tsk_handles.push(task);
                                }
                                else
                                {
                                    error!( "{:?} Service not implemented ATM for ch: {}",self.srv_type, ch_id);
                                }
                            }
                            else if proto_srv.media_sink_service.video_configs.len()>0
                            {

                                let _=match proto_srv.media_sink_service.video_configs[0].codec_resolution() {
                                    VideoCodecResolutionType::VIDEO_800x480=>{ self.sdr_video_codec_params.bitrate =4_000_000; self.sdr_video_codec_params.res_w=800; self.sdr_video_codec_params.res_h=480; Video_800x480},
                                    VideoCodecResolutionType::VIDEO_720x1280=>{ self.sdr_video_codec_params.bitrate =8_000_000; self.sdr_video_codec_params.res_w=1280; self.sdr_video_codec_params.res_h=720; Video_720x1280},
                                    VideoCodecResolutionType::VIDEO_1080x1920=>{ self.sdr_video_codec_params.bitrate =16_000_000; self.sdr_video_codec_params.res_w=1920; self.sdr_video_codec_params.res_h=1080; Video_1080x1920},
                                    _=>{ self.sdr_video_codec_params.bitrate =4_000_000; self.sdr_video_codec_params.res_w=800; self.sdr_video_codec_params.res_h=480; Video_800x480},
                                };
                                let _=match proto_srv.media_sink_service.video_configs[0].video_codec_type() {
                                    MediaCodecType::MEDIA_CODEC_VIDEO_H264_BP=>VIDEO_H264_BP,
                                    MediaCodecType::MEDIA_CODEC_VIDEO_H265=>VIDEO_H265,
                                    MediaCodecType::MEDIA_CODEC_AUDIO_PCM=>AUDIO_PCM,
                                    _=>VIDEO_H264_BP,
                                };
                                let _=match proto_srv.media_sink_service.video_configs[0].frame_rate() {
                                    VideoFrameRateType::VIDEO_FPS_60=>{ self.sdr_video_codec_params.fps=60; FPS_60},
                                    VideoFrameRateType::VIDEO_FPS_30=>{ self.sdr_video_codec_params.fps=30; FPS_30},
                                    _=>{ self.sdr_video_codec_params.fps=30; FPS_30},
                                };
                                //ovveride from config file
                                if self.config.video_bitrate > 0
                                {
                                    self.sdr_video_codec_params.bitrate=self.config.video_bitrate;
                                }
                                self.sdr_video_codec_params.dpi=proto_srv.media_sink_service.video_configs[0].density() as i32;
                                self.sdr_video_codec_params.sid=ch_id as u8;

                                let service = SrvMediaSinkVideoStreaming::new(ch_id as i8, self.hu_tx.clone(), self.video_server_ready.clone(), self.sdr_video_codec_params.clone(), self.cancel.clone(), self.config.ignore_media_ack, true);
                                let (service_handle, task) = service.start();
                                self.add_service(service_handle);
                                self.srv_tsk_handles.push(task);
                            }
                            else {
                                error!( "{:?} Service not implemented ATM for ch: {}",self.srv_type, ch_id);
                            }
                        }
                        else if proto_srv.media_source_service.is_some()
                        {
                            let service = SrvMediaSource::new(ch_id as i8, self.hu_tx.clone());
                            let (service_handle, task) = service.start(self.cancel.clone());
                            self.add_service(service_handle);
                            self.srv_tsk_handles.push(task);
                        }
                        else if proto_srv.sensor_source_service.is_some()
                        {
                            if proto_srv.sensor_source_service.sensors.len()>0
                            {
                                for s in proto_srv.sensor_source_service.sensors.clone() {
                                    if let Ok(st) = SensorType::try_from(s.sensor_type() as i32)
                                    {
                                        self.sdr_sensors.push(st);
                                    }
                                }
                            }
                            let service = SrvSensorSource::new(ch_id as i8, self.hu_tx.clone(), self.sdr_sensors.clone());
                            let (service_handle, task) = service.start(self.cancel.clone());
                            self.add_service(service_handle);
                            self.srv_tsk_handles.push(task);

                        }
                        else if proto_srv.input_source_service.is_some()
                        {
                            let screen_size=ScrcpySize{ width: self.sdr_video_codec_params.res_w as u16, height: self.sdr_video_codec_params.res_h as u16 };
                            self.sdr_keys=proto_srv.input_source_service.keycodes_supported.iter().cloned().collect();
                            self.sdr_control_server_sid= ch_id as u8;
                            let service = SrvInputSource::new(ch_id as i8, self.hu_tx.clone(),self.control_server_ready.clone(), self.sdr_keys.clone(), screen_size, self.config.scrcpy_screen_off, self.cancel.clone());
                            let (service_handle, task) = service.start();
                            self.add_service(service_handle);
                            self.srv_tsk_handles.push(task);
                        }
                        else if proto_srv.vendor_extension_service.is_some()
                        {
                            let service = SrvVendorExtension::new(ch_id as i8, self.hu_tx.clone());
                            let (service_handle, task) = service.start(self.cancel.clone());
                            self.add_service(service_handle);
                            self.srv_tsk_handles.push(task);
                        }
                        else if proto_srv.bluetooth_service.is_some()
                        {
                            let service = SrvBluetooth::new(ch_id as i8, self.hu_tx.clone());
                            let (service_handle, task) = service.start(self.cancel.clone());
                            self.add_service(service_handle);
                            self.srv_tsk_handles.push(task);
                        }
                        else
                        {
                            error!( "{:?} Service not implemented ATM for ch: {}",self.srv_type, ch_id);
                        }
                    }
                    info!( "{:?} Sending AudioFocus request...",self.srv_type);
                    let mut focus_req= AudioFocusRequestNotification::new();
                    focus_req.set_request(AudioFocusRequestType::AUDIO_FOCUS_GAIN);

                    let mut payload: Vec<u8>=focus_req.write_to_bytes()?;
                    payload.insert(0,((ControlMessageType::MESSAGE_AUDIO_FOCUS_REQUEST as u16) >> 8) as u8);
                    payload.insert( 1,((ControlMessageType::MESSAGE_AUDIO_FOCUS_REQUEST as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: 0,
                        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                        error!( "{:?} tls proxy send error",self.srv_type);
                    };
                }
                else {
                    error!( "{:?} ServiceDiscoveryResponse couldn't be parsed",self.srv_type);
                    return Err(Box::new("ServiceDiscoveryResponse couldn't be parsed")).expect("ServiceDiscoveryResponse");
                }
                info!( "{:?} ServiceDiscovery done, starting AA Mirror loop",self.srv_type);
            }
            else if message_id == ControlMessageType::MESSAGE_PING_REQUEST as i32
            {
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = PingRequest::parse_from_bytes(&data) {
                    let mut pingrsp= PingResponse::new();
                    pingrsp.set_timestamp(msg.timestamp());
                    let mut payload: Vec<u8>=pingrsp.write_to_bytes()?;
                    payload.insert(0,((ControlMessageType::MESSAGE_PING_RESPONSE as u16) >> 8) as u8);
                    payload.insert( 1,((ControlMessageType::MESSAGE_PING_RESPONSE as u16) & 0xff) as u8);
                    let pkt_rsp = Packet {
                        channel: 0,
                        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = self.hu_tx.send(pkt_rsp).await{
                        error!( "{:?} tls proxy send error",self.srv_type);
                    };
                }
                else {
                    error!( "{:?} PingRequest couldn't be parsed",self.srv_type);
                }

            }
            else if message_id == ControlMessageType::MESSAGE_AUDIO_FOCUS_NOTIFICATION as i32
            {
                let data = &pkt.payload[2..]; // start of message data, without message_id
                if let Ok(msg) = AudioFocusNotification::parse_from_bytes(&data) {
                    info!( "{:?} AUDIO_FOCUS_STATE received is: {:?}", self.srv_type, msg.focus_state());
                    if !self.ch_opened && ((msg.focus_state() == AudioFocusStateType::AUDIO_FOCUS_STATE_GAIN) || (msg.focus_state() == AudioFocusStateType::AUDIO_FOCUS_STATE_GAIN_TRANSIENT))
                    {
                        info!( "{} CMD OPEN_CHANNEL will be done next",self.srv_type);
                        tokio::time::sleep(Duration::from_millis(HU_CONFIG_DELAY_MS)).await; //reconfiguration time for HU
                        //Open CH for all
                        for service in self.sdr_services.iter().flatten() {
                            info!( "{:?} Send custom CMD_OPEN_CH for ch {}",self.srv_type, service.sid());
                            let mut payload= Vec::new();
                            payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                            payload.extend_from_slice(&(CustomCommand::CMD_OPEN_CH as u16).to_be_bytes());
                            let msg = Packet {
                                channel: service.sid() as u8,
                                flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                final_length: None,
                                payload,
                            };
                            service.enqueue_message(msg)?;
                        }
                        self.ch_opened=true;
                    }
                    else if self.ch_opened
                    {
                        //proxy to Audio channel, we have to manage focus there not in control channel
                        if let Some(Some(service)) = self.sdr_services.get(self.sdr_audio_codec_params.sid as usize) {
                            pkt.channel=self.sdr_audio_codec_params.sid;//change its sid
                            service.enqueue_message(pkt)?;
                        }
                        else
                        {
                            error!( "{:?} Invalid channel {}",self.srv_type, pkt.channel);
                        }
                    }

                }
                else
                {
                    error!( "{:?} AudioFocusNotification couldn't be parsed",self.srv_type);
                }
            }
            else if message_id == ControlMessageType::MESSAGE_UNEXPECTED_MESSAGE as i32
            {
                error!( "{:?} MESSAGE_UNEXPECTED_MESSAGE received from HU",self.srv_type);
            }
            else
            {
                error!( "{:?} Unmanaged message ID: {}",self.srv_type, message_id);
            }
        }
        else
        {
            //Service channel
            if let Some(service) = self.sdr_services.get(pkt.channel as usize).and_then(|s| s.as_ref()) {
                service.enqueue_message(pkt)?;
            }
            else
            {
                error!( "{:?} Invalid channel {}",self.srv_type, pkt.channel);
            }
        }
        Ok(())
    }
    async fn start_adb_servers(&mut self) -> Result<()> {
        //Start ADB server first
        info!( "{:?} All 3 SCRCPY servers ready to connect, Send notification to ADB server",self.srv_type);
        self.start_adb_server.notify_one();
        //this waiting time is MANDATORY, otherwise we get error on video socket, why???
        tokio::time::sleep(Duration::from_millis(500)).await;//give some time to start server and sockets
        if(self.sdr_video_codec_params.sid > 0)
        {
            let mut payload= Vec::new();
            payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
            payload.extend_from_slice(&(CustomCommand::CMD_START_VIDEO_RECORDING as u16).to_be_bytes());
            let msg = Packet {
                channel: self.sdr_video_codec_params.sid,
                flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                final_length: None,
                payload,
            };
            info!( "{:?} Send custom CMD_START_VIDEO_RECORDING for ch {}",self.srv_type, self.sdr_video_codec_params.sid);
            if let Some(service) = self.sdr_services.get(self.sdr_video_codec_params.sid as usize).and_then(|s| s.as_ref()) {
                service.enqueue_message(msg)?;
            }
            else
            {
                error!( "{:?} Invalid channel {} vor video service",self.srv_type, self.sdr_video_codec_params.sid);
                self.cancel.cancel();
            }
        }
        else
        {
            error!( "{:?} Invalid channel {} vor video service",self.srv_type, self.sdr_video_codec_params.sid);
            self.cancel.cancel();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;//give time to connect
        if(self.sdr_audio_codec_params.sid > 0)
        {
            let mut payload= Vec::new();
            payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
            payload.extend_from_slice(&(CustomCommand::CMD_START_AUDIO_RECORDING as u16).to_be_bytes());
            let msg = Packet {
                channel: self.sdr_audio_codec_params.sid,
                flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                final_length: None,
                payload,
            };
            info!( "{:?} Send custom CMD_START_VIDEO_RECORDING for ch {}",self.srv_type, self.sdr_audio_codec_params.sid);
            if let Some(service) = self.sdr_services.get(self.sdr_audio_codec_params.sid as usize).and_then(|s| s.as_ref()) {
                service.enqueue_message(msg)?;
            }
            else
            {
                error!( "{:?} Invalid channel {} vor video service",self.srv_type, self.sdr_audio_codec_params.sid);
                self.cancel.cancel();
            }
        }
        else
        {
            error!( "{:?} Invalid channel {} vor audio service",self.srv_type, self.sdr_audio_codec_params.sid);
            self.cancel.cancel();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;//give time to connect
        if(self.sdr_control_server_sid > 0)
        {
            let mut payload= Vec::new();
            payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
            payload.extend_from_slice(&(CustomCommand::CMD_START_CONTROL_SERVER as u16).to_be_bytes());
            let msg = Packet {
                channel: self.sdr_control_server_sid,
                flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                final_length: None,
                payload,
            };
            info!( "{:?} Send custom CMD_START_VIDEO_RECORDING for ch {}",self.srv_type, self.sdr_control_server_sid);
            if let Some(service) = self.sdr_services.get(self.sdr_control_server_sid as usize).and_then(|s| s.as_ref()) {
                service.enqueue_message(msg)?;
            }
            else
            {
                error!( "{:?} Invalid channel {} vor video service",self.srv_type, self.sdr_control_server_sid);
                self.cancel.cancel();
            }
        }
        else
        {
            error!( "{:?} Invalid channel {} vor control service",self.srv_type, self.sdr_control_server_sid);
            self.cancel.cancel();
        }
        Ok(())
    }
    fn add_service(&mut self, service: AAService) {
        let sid = service.sid() as usize;
        if self.sdr_services.len() <= sid
        {
            self.sdr_services.resize_with(sid + 1, || None);
        }
        self.sdr_services[sid] = Some(service);
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
