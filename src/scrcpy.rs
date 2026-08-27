use std::cmp::min;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::net::{SocketAddr, IpAddr, Ipv4Addr};
use std::time::Duration;
use std::time::Instant;
use bytes::{BytesMut, Bytes, Buf};
use libc::sigdelset;
use log::debug;
use openssl::pkey::Public;
use serde::{Deserialize, Serialize};
use simplelog::{error, info};
use tokio::process::Command;
use tokio::sync::mpsc::Receiver;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::net::TcpStream;
use crate::aa_services::{AAService, AudioConfig, AudioStreamingParams, MediaCodec, SensorType, ServiceType, SrvSensorSource, VideoStreamingParams};
use crate::{adb, channel_manager};
use crate::channel_manager::{ChannelProxyHandle, Packet, TlsPacketProxy, ENCRYPTED, FRAME_TYPE_CONTROL, FRAME_TYPE_FIRST, FRAME_TYPE_LAST};
use crate::config::{AppConfig, SharedConfig, MAX_DATA_LEN, SCRCPY_METADATA_HEADER_LEN, SCRCPY_PORT, SCRCPY_VERSION};
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use protos::*;
use protos::ControlMessageType::{self, *};
use protobuf::{Message};
use tokio::io::{AsyncBufReadExt,AsyncWriteExt, AsyncRead, AsyncReadExt};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use crate::config_types::HexdumpLevel;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const NAME: &str = "<i><bright-black> scrcpy: </>";

#[derive(Serialize, Deserialize, Copy, Clone, Debug)]
pub enum AndroidTouchEvent
{
    Down=0,
    Up=1,
    Move=2,
    Scroll=8,
    BackOrScreenOn,
}
#[derive(Serialize, Deserialize, Copy, Clone, Debug)]
pub enum AndroidKeyEvent
{
    Down=0,
    Up=1,
}
#[derive(Serialize, Deserialize, Copy, Clone, Debug)]
pub enum ScrcpyControlMessageType
{
    InjectKeycode,
    InjectTouchEvent=2,
    InjectScrollEvent,
    BackOrScreenOn,
    SetDisplayPower=10,
    RotateDevice=11,
}
#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpySetDisplayPowerEvent {
    pub on: bool,
}
#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpyTouchEvent {
    pub action: u8,
    pub pointer_id: u64,
    pub position:ScrcpyPosition,
    pub pressure:u16,
    pub action_button:i32,
    pub buttons:i32,
}

impl ScrcpyTouchEvent {
    /// Serialize struct into big-endian bytes using BytesMut
    fn to_be_bytes(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&self.action.to_be_bytes());
        buf.extend_from_slice(&self.pointer_id.to_be_bytes());
        buf.extend_from_slice(&self.position.to_be_bytes());
        buf.extend_from_slice(&self.pressure.to_be_bytes());
        buf.extend_from_slice(&self.action_button.to_be_bytes());
        buf.extend_from_slice(&self.buttons.to_be_bytes());
        buf.to_vec() // convert BytesMut to Vec<u8>
    }
}


#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpyKeyEvent {
    pub action: u8,
    pub key_code: i32,
    pub repeat:i32,
    pub metastate:i32,
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpyScrollEvent {
    pub position:ScrcpyPoint,
    pub screen_size: ScrcpySize,
    pub hscroll: i16,
    pub vscroll:i16,
    pub buttons:i32,
}

impl ScrcpyKeyEvent {
    /// Serialize struct into big-endian bytes using BytesMut
    fn to_be_bytes(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&self.action.to_be_bytes());
        buf.extend_from_slice(&self.key_code.to_be_bytes());
        buf.extend_from_slice(&self.repeat.to_be_bytes());
        buf.extend_from_slice(&self.metastate.to_be_bytes());
        buf.to_vec() // convert BytesMut to Vec<u8>
    }
}

impl ScrcpyScrollEvent {
    /// Serialize struct into big-endian bytes using BytesMut
    fn to_be_bytes(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&self.position.to_be_bytes());
        buf.extend_from_slice(&self.screen_size.to_be_bytes());
        buf.extend_from_slice(&self.hscroll.to_be_bytes());
        buf.extend_from_slice(&self.vscroll.to_be_bytes());
        buf.extend_from_slice(&self.buttons.to_be_bytes());
        buf.to_vec() // convert BytesMut to Vec<u8>
    }
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpyPosition {
    pub point: ScrcpyPoint,
    pub screen_size: ScrcpySize,
}

impl ScrcpyPosition {
    /// Serialize struct into big-endian bytes using BytesMut
    fn to_be_bytes(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&self.point.to_be_bytes());
        buf.extend_from_slice(&self.screen_size.to_be_bytes());
        buf.to_vec() // convert BytesMut to Vec<u8>
    }
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpyPoint {
    pub x: i32,
    pub y: i32,
}

impl ScrcpyPoint {
    /// Serialize struct into big-endian bytes using BytesMut
    fn to_be_bytes(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&self.x.to_be_bytes());
        buf.extend_from_slice(&self.y.to_be_bytes());
        buf.to_vec() // convert BytesMut to Vec<u8>
    }
}
#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpySize {
    pub width: u16,
    pub height: u16,
}

impl ScrcpySize {
    /// Serialize struct into big-endian bytes using BytesMut
    fn to_be_bytes(&self) -> Vec<u8> {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&self.width.to_be_bytes());
        buf.extend_from_slice(&self.height.to_be_bytes());
        buf.to_vec() // convert BytesMut to Vec<u8>
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ScrcpyVideoCodecInfo {
    pub codec_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ScrcpySessionMeta {
    pub flags: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, Copy)]
pub struct ScrcpyMediaHeader {
    pub size:usize,
    pub timestamp: u64,
    pub config:bool,
    pub keyframe:bool,
}
pub struct ScrcpyMediaReader {
    stream: TcpStream,
    buf: BytesMut,   // reusable buffer
}

impl ScrcpyMediaReader {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buf: BytesMut::with_capacity(256 * 1024),
        }
    }

    /// Read exactly N bytes into internal buffer
    async fn read_exact_into_buf(&mut self, size: usize) -> io::Result<()> {
        self.buf.clear();
        self.buf.reserve(size);

        let mut read_total = 0;
        let mut tmp = [0u8; 64 * 1024];

        while read_total < size {
            let to_read = (size - read_total).min(tmp.len());

            let n = self.stream.read(&mut tmp[..to_read]).await?;

            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF",
                ));
            }

            self.buf.extend_from_slice(&tmp[..n]);
            read_total += n;
        }

        Ok(())
    }

    pub async fn read_chunks(&mut self) -> tokio::io::Result<Option<(ScrcpyMediaHeader, Vec<Bytes>)>> {
        // 1. Read header
        self.read_exact_into_buf(SCRCPY_METADATA_HEADER_LEN).await?;

        let header = ScrcpyMediaReader::parse_header(&self.buf[..SCRCPY_METADATA_HEADER_LEN]);

        // 2. Read payload exactly
        self.read_exact_into_buf(header.size).await?;

        // 3. Split into chunks WITHOUT copying (zero-copy via Bytes)
        let mut chunks = Vec::with_capacity((header.size + MAX_DATA_LEN - 1) / MAX_DATA_LEN);

        let mut offset = 0;
        while offset < header.size {
            let end = (offset + MAX_DATA_LEN).min(header.size);

            let slice = self.buf[offset..end].to_vec(); // unavoidable copy with tokio_uring
            chunks.push(Bytes::from(slice));

            offset = end;
        }

        Ok(Some((header, chunks)))
    }

    fn parse_header(buf: &[u8]) -> ScrcpyMediaHeader {
        use std::convert::TryInto;
        //debug!("ScrcpyMediaReader raw header bytes: {:02x?}",&buf);
        let pts = u64::from_be_bytes(buf[..8].try_into().unwrap());
        let size = u32::from_be_bytes(buf[8..12].try_into().unwrap()) as usize;
        let rec_ts = pts & 0x3FFF_FFFF_FFFF_FFFFu64;
        //let session_frame = (pts & 0x8000_0000_0000_0000u64) != 0;//this is for Session metadata only
        let config_frame = (pts & 0x4000_0000_0000_0000u64) != 0;
        let key_frame = (pts & 0x2000_0000_0000_0000u64) != 0;
        ScrcpyMediaHeader {size:size, timestamp: rec_ts, config: config_frame, keyframe: key_frame }
    }

    pub async fn read_video_codec_info(&mut self)  -> io::Result<ScrcpyVideoCodecInfo> {
        const META_SIZE: usize = 4;
        // Fill internal buffer (no allocation)
        self.read_exact_into_buf(META_SIZE).await?;

        let metadata = &self.buf[..META_SIZE];


        let mut codec_id=String::from_utf8_lossy(&metadata[0..4]).to_string();
        codec_id=codec_id.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').collect();

        Ok(ScrcpyVideoCodecInfo {
            codec_id
        })
    }

    pub async fn read_video_session_info(&mut self)  -> io::Result<ScrcpySessionMeta> {
        const META_SIZE: usize = 12;
        // Fill internal buffer (no allocation)
        self.read_exact_into_buf(META_SIZE).await?;

        let metadata = &self.buf[..META_SIZE];

        // ---- flags ----
        let flags = u32::from_be_bytes(
            metadata[0..4].try_into().unwrap()
        );

        // ---- width ----
        let width = u32::from_be_bytes(
            metadata[4..8].try_into().unwrap()
        );

        // ---- height ----
        let height = u32::from_be_bytes(
            metadata[8..12].try_into().unwrap()
        );

        Ok(ScrcpySessionMeta {
            flags,
            width,
            height,
        })
    }

    pub async fn read_audio_codec_info(&mut self)  -> io::Result<String> {
        const META_SIZE: usize = 4;
        // Fill internal buffer (no allocation)
        self.read_exact_into_buf(META_SIZE).await?;

        let metadata = &self.buf[..META_SIZE];

        let mut codec_id=String::from_utf8_lossy(&metadata[0..4]).to_string();
        codec_id=codec_id.chars()
            .filter(|c| c.is_ascii_graphic() || *c == ' ')
            .collect();

        Ok(codec_id)
    }
}

pub struct VideoServer {
    sid:u8,
    hu_tx: Sender<Packet>,
    cancel: CancellationToken,
    //private members
    paused: Arc<AtomicBool>,
}
pub struct VideoServerHandle {
    ack_rx: Receiver<()>,
    paused: Arc<AtomicBool>, // shared with the task
}
pub enum VideoServerState {
    Created(VideoServer),
    Running(VideoServerHandle),
}
impl VideoServerHandle {
    pub fn ack(&mut self) {
        match self.ack_rx.try_recv() {
            Ok(_) => {}
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => {
                error!("ACK channel dropped");
            }
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
}
impl VideoServer {
    pub fn new(sid:u8, hu_tx: Sender<Packet>, cancel: CancellationToken,) -> Self {

        Self {
            sid,
            hu_tx,
            cancel,
            paused: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn start(mut self, max_unack :u8) -> VideoServerHandle {
        let ignore_ack= max_unack == 0;
        let (ack_tx, ack_rx) = mpsc::channel::<()>(max_unack.max(1) as usize);
        let paused = self.paused.clone(); // clone Arc before moving self
        tokio::spawn(async move {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), SCRCPY_PORT as u16);
            let stream = match timeout(
                Duration::from_secs(5),
                TcpStream::connect(addr),
            ).await {
                Ok(Ok(stream)) =>
                    {
                        info!("Starting video server!");
                        //stream.set_nodelay(true)?;//we recieve only, we don't need it
                        let mut reader=ScrcpyMediaReader::new(stream);
                        //codec metadata
                        match reader.read_video_codec_info().await {
                            Ok(info) => {
                                info!("SCRCPY Video metadata decoded: id={}", info.codec_id);
                                if info.codec_id != "h264".to_string() {
                                    error!("SCRCPY Invalid Video codec configuration");
                                    return ;
                                }
                            }
                            Err(e) => {
                                error!("SCRCPY Video reading error: {:?}",e);
                                self.cancel.cancel();
                                return ;
                            }
                        }
                        match reader.read_video_session_info().await {
                            Ok(info) => {
                                info!("SCRCPY Video Session metadata decoded: flags={}, res_w={}, res_h={}", info.flags, info.width, info.height);
                            }
                            Err(e) => {
                                error!("SCRCPY Video Session metadata reading error: {:?}",e);
                                return ;
                            }
                        }
                        debug!("SCRCPY Video entering main loop");
                        let mut dbg_count=0;
                        while !self.cancel.is_cancelled()
                        {
                            //Read video frames from SCRCPY server
                            match reader.read_chunks().await {
                                Ok(Some((media_header, chunks))) => {
                                    //let rd_len = header.size ;
                                    //let dbg_len = min(media_header.size, 16);
                                    let raw_bytes = chunks.first().map(|chunk| &chunk[..chunk.len().min(media_header.size).min(16)]).unwrap_or(&[]);
                                    if dbg_count <  10
                                    {
                                        debug!("Video task got frame config={:?}, ts={}, act size: {}, raw bytes: {:02x?}",media_header.config, media_header.timestamp, media_header.size, raw_bytes);
                                        dbg_count += 1;
                                    }
                                    if self.paused.load(Ordering::Relaxed) || media_header.size <=0
                                    {
                                        continue;
                                    }
                                    if !media_header.config
                                    {
                                        //wait for ACK
                                        if !ignore_ack
                                        {
                                            if let Err(e) = ack_tx.send(()).await {
												error!("scrcpy video ack send failed: {:?}", e);
												return;
											}
                                        }

                                    }
                                    let pk_header_size = if media_header.config {
                                        2
                                    } else {
                                        2 + 8
                                    };
                                    //send all chunks
                                    if chunks.len() > 1
                                    {
                                        //fragmented packet
                                        for (i,chunk) in chunks.iter().enumerate()
                                        {

                                            let mut flags:u8;
                                            let mut total_len = None;
                                            let mut payload;
                                            if i==0
                                            {
                                                flags = ENCRYPTED | FRAME_TYPE_FIRST;
                                                total_len=Some((media_header.size + pk_header_size) as u32);
                                                payload = Vec::with_capacity(pk_header_size + chunk.len());
                                                if media_header.config
                                                {
                                                    payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16).to_be_bytes());
                                                    payload.extend_from_slice(&chunk);
                                                }
                                                else
                                                {
                                                    payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_DATA as u16).to_be_bytes());
                                                    payload.extend_from_slice(&media_header.timestamp.to_be_bytes());
                                                    payload.extend_from_slice(&chunk);
                                                }
                                            }
                                            else if i== (chunks.len() - 1)
                                            {
                                                flags = ENCRYPTED | FRAME_TYPE_LAST;
                                                payload = chunk.to_vec();
                                            }
                                            else
                                            {
                                                flags = ENCRYPTED;
                                                payload = chunk.to_vec();
                                            }


                                            let pkt_rsp = Packet {
                                                channel: self.sid,
                                                flags: flags,
                                                final_length: total_len,
                                                payload,
                                            };
                                            if let Err(e) = self.hu_tx.send(pkt_rsp).await {
												error!("Error sending video chunk: {:?}", e);
												return;
											}
                                        }
                                    }
                                    else {
                                        //single packet
                                        let mut payload = Vec::with_capacity(pk_header_size + chunks[0].len());
                                        if media_header.config
                                        {
                                            payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16).to_be_bytes());
                                            payload.extend_from_slice(&chunks[0]);
                                        }
                                        else
                                        {
                                            payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_DATA as u16).to_be_bytes());
                                            payload.extend_from_slice(&media_header.timestamp.to_be_bytes());
                                            payload.extend_from_slice(&chunks[0]);
                                        }
                                        let pkt_rsp = Packet {
                                            channel: self.sid,
                                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                            final_length: None,
                                            payload,
                                        };
                                        if let Err(e) = self.hu_tx.send(pkt_rsp).await {
											error!("Error sending video chunk: {:?}", e);
											return;
										}
                                    }

                                }
                                Ok(None) => {
                                    error!("scrcpy video read failed");
                                    return ;
                                }
                                Err(e) => {
                                    error!("scrcpy video read failed: {}", e);
                                    break;
                                }
                            }

                        }
                    }

                Ok(Err(e)) => {
                    error!("VideoServer TCP connect failed: {}", e);
                    return;
                }

                Err(_) => {
                    error!("VideoServer TCP connect timeout");
                    return;
                }
            };
            return;
        });
        VideoServerHandle { ack_rx, paused }
    }
}

pub struct AudioServer {
    sid:u8,
    hu_tx: Sender<Packet>,
    cancel: CancellationToken,
    //private members
    paused: Arc<AtomicBool>,
}
pub struct AudioServerHandle {
    ack_rx: Receiver<()>,
    paused: Arc<AtomicBool>, // shared with the task
}
pub enum AudioServerState {
    Created(AudioServer),
    Running(AudioServerHandle),
}
impl AudioServerHandle {
    pub fn ack(&mut self) {
        match self.ack_rx.try_recv() {
            Ok(_) => {}
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => {
                error!("ACK channel dropped");
            }
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
}
impl AudioServer {
    pub fn new(sid:u8, hu_tx: Sender<Packet>, cancel: CancellationToken) -> Self {
        Self {
            sid,
            hu_tx,
            cancel,
            paused: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn start(mut self, max_unack:u8) -> AudioServerHandle {
        let ignore_ack= max_unack==0;
        let (ack_tx, ack_rx) = mpsc::channel::<()>(max_unack.max(1) as usize);
        let paused = self.paused.clone(); // clone Arc before moving self
        tokio::spawn(async move {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), SCRCPY_PORT as u16);
            let stream = match timeout(
                Duration::from_secs(5),
                TcpStream::connect(addr),
            ).await {
                Ok(Ok(stream)) =>
                    {
                        info!("Starting audio server!");
                        let mut reader=ScrcpyMediaReader::new(stream);
                        //codec metadata
                        match reader.read_audio_codec_info().await {
                            Ok(codec_id) => {
                                info!("SCRCPY Audio metadata decoded: id={}", codec_id);
                                if codec_id != "raw" && codec_id != "aac" {
                                    error!("SCRCPY Invalid Audio codec configuration");
                                    self.cancel.cancel();
                                    return ;
                                }
                            }
                            Err(e) => {
                                error!("SCRCPY Audio reading error: {:?}",e);
                                self.cancel.cancel();
                                return ;
                            }
                        }
                        debug!("SCRCPY Audio entering main loop");
                        let mut dbg_count=0;
                        while !self.cancel.is_cancelled()
                        {
                            //Read audio frames from SCRCPY server
                            match reader.read_chunks().await {
                                Ok(Some((media_header, chunks))) => {
                                    //let rd_len = header.size ;
                                    //let dbg_len = min(media_header.size, 16);
                                    let raw_bytes = chunks.first().map(|chunk| &chunk[..chunk.len().min(media_header.size).min(16)]).unwrap_or(&[]);
                                    if dbg_count <  10
                                    {
                                        debug!("Video task got frame config={:?}, ts={}, act size: {}, raw bytes: {:02x?}",media_header.config, media_header.timestamp, media_header.size, raw_bytes);
                                        dbg_count += 1;
                                    }
                                    if self.paused.load(Ordering::Relaxed) || media_header.size <=0
                                    {
                                        continue;
                                    }
                                    if !media_header.config
                                    {
                                        //wait for ACK
                                        if !ignore_ack
                                        {
                                            if let Err(e) = ack_tx.send(()).await {
												error!("scrcpy audio ack send failed: {:?}", e);
												return;
											}
                                        }

                                    }
                                    let pk_header_size = if media_header.config {
                                        2
                                    } else {
                                        2 + 8
                                    };
                                    //send all chunks
                                    if chunks.len() > 1
                                    {
                                        //fragmented packet
                                        for (i,chunk) in chunks.iter().enumerate()
                                        {

                                            let mut flags:u8;
                                            let mut total_len = None;
                                            let mut payload;
                                            if i==0
                                            {
                                                flags = ENCRYPTED | FRAME_TYPE_FIRST;
                                                total_len=Some((media_header.size + pk_header_size) as u32);
                                                payload = Vec::with_capacity(pk_header_size + chunk.len());
                                                if media_header.config
                                                {
                                                    payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16).to_be_bytes());
                                                    payload.extend_from_slice(&chunk);
                                                }
                                                else
                                                {
                                                    payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_DATA as u16).to_be_bytes());
                                                    payload.extend_from_slice(&media_header.timestamp.to_be_bytes());
                                                    payload.extend_from_slice(&chunk);
                                                }
                                            }
                                            else if i== (chunks.len() - 1)
                                            {
                                                flags = ENCRYPTED | FRAME_TYPE_LAST;
                                                payload = chunk.to_vec();
                                            }
                                            else
                                            {
                                                flags = ENCRYPTED;
                                                payload = chunk.to_vec();
                                            }


                                            let pkt_rsp = Packet {
                                                channel: self.sid,
                                                flags: flags,
                                                final_length: total_len,
                                                payload,
                                            };
                                            if let Err(e) = self.hu_tx.send(pkt_rsp).await {
												error!("Error sending audio chunk: {:?}", e);
												return;
											}
                                        }
                                    }
                                    else {
                                        //single packet
                                        let mut payload = Vec::with_capacity(pk_header_size + chunks[0].len());
                                        if media_header.config
                                        {
                                            payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16).to_be_bytes());
                                            payload.extend_from_slice(&chunks[0]);
                                        }
                                        else
                                        {
                                            payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_DATA as u16).to_be_bytes());
                                            payload.extend_from_slice(&media_header.timestamp.to_be_bytes());
                                            payload.extend_from_slice(&chunks[0]);
                                        }
                                        let pkt_rsp = Packet {
                                            channel: self.sid,
                                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                            final_length: None,
                                            payload,
                                        };
                                        if let Err(e) = self.hu_tx.send(pkt_rsp).await {
											error!("Error sending audio chunk: {:?}", e);
											return;
										}
                                    }

                                }
                                Ok(None) => {
                                    error!("scrcpy audio read failed");
                                    return ;
                                }
                                Err(e) => {
                                    error!("scrcpy audio read failed: {}", e);
                                    break;
                                }
                            }

                        }
                    }

                Ok(Err(e)) => {
                    error!("AudioServer TCP connect failed: {}", e);
                    return;
                }

                Err(_) => {
                    error!("AudioServer TCP connect timeout");
                    return;
                }
            };
            return;
        });
        AudioServerHandle { ack_rx, paused }
    }
}

pub struct ControlServer {
    sid:u8,
    hu_tx: Sender<Packet>,
    screen_size:ScrcpySize,
    cfg_screen_off:bool,
    cancel: CancellationToken,
    //private members
	last_touched_point:ScrcpyPoint,
	pkt_tx: Sender<Packet>,
    pkt_rx: Receiver<Packet>,
}

pub enum ControlServerState {
    Created(ControlServer),
    Running(ControlServerHandle),
}

pub struct ControlServerHandle {
    pkt_tx: Sender<Packet>,
}
impl ControlServerHandle {
    pub async fn enque_msg(&self, msg: Packet) {
        if let Err(e) = self.pkt_tx.send(msg).await {
            error!("scrcpy control send failed: {:?}", e);
        }
    }
}
impl ControlServer {
    pub fn new(sid:u8, hu_tx: Sender<Packet>, screen_size:ScrcpySize, cfg_screen_off:bool, cancel: CancellationToken,) -> Self {
        let (pkt_tx, mut pkt_rx) = mpsc::channel::<Packet>(5);
        Self {
            sid,
            hu_tx,
			screen_size,
			cfg_screen_off,
            cancel,
			last_touched_point:ScrcpyPoint{x:0,y:0},
			pkt_rx,
			pkt_tx,
        }
    }
    pub fn start(mut self,) -> ControlServerHandle {
        let pkt_tx = self.pkt_tx.clone(); // clone before moving self into the task
        tokio::spawn(async move {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), SCRCPY_PORT as u16);
            let stream = match timeout(Duration::from_secs(5), TcpStream::connect(addr),
            ).await {
                Ok(Ok(mut stream)) =>
                    {
                        info!("Starting control server!");
                        if let Err(e) = stream.set_nodelay(true) {
                            error!("Failed to set TCP_NODELAY: {}", e);
                        }
						if self.cfg_screen_off {
							let mut payload: Vec<u8> = Vec::new();
							payload.push(ScrcpyControlMessageType::SetDisplayPower as u8);
							payload.push(0);
							if let Err(e) = stream.write_all(&payload).await {
								error!("tsk_scrcpy_control send error: {}", e);
							}
						}
                        debug!("SCRCPY Control entering main loop");
                        while !self.cancel.is_cancelled()
                        {
                            match self.pkt_rx.recv().await {
								Some(pkt) => {
									// Received a packet
									let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into().unwrap()).into();
									info!("tsk_scrcpy_control Received command id {:?}", message_id);
									if message_id == InputMessageId::INPUT_MESSAGE_INPUT_REPORT  as i32
									{
										let data = &pkt.payload[2..]; // start of message data, without message_id
										if  let Ok(rsp) = InputReport::parse_from_bytes(&data) {
											//info!( "tsk_scrcpy_control InputReport received: {:?}", rsp);
											if rsp.touch_event.is_some()
											{
												let touch_action = rsp.touch_event.action();
												for (_,touch_ev) in rsp.touch_event.pointer_data.iter().enumerate() {
													let touch_x = touch_ev.x();
													let touch_y = touch_ev.y();
													let pointer_id = touch_ev.pointer_id();


													let mut _action: u8;
													if touch_action == PointerAction::ACTION_DOWN
													{
														_action = AndroidTouchEvent::Down as u8;
													} else if touch_action == PointerAction::ACTION_UP
													{
														_action = AndroidTouchEvent::Up as u8;
														self.last_touched_point = ScrcpyPoint { x: touch_x as i32, y: touch_y as i32 };
													} else if touch_action == PointerAction::ACTION_MOVED
													{
														_action = AndroidTouchEvent::Move as u8;
													} else {
														error!( "tsk_scrcpy_control Received invalid touchscreen action");
														continue;
													}
													let pt = ScrcpyPoint { x: touch_x as i32, y: touch_y as i32 };
													//let sz = ScrcpySize { width: video_params.res_w as u16, height: video_params.res_h as u16 };
													let pos = ScrcpyPosition { point: pt, screen_size: self.screen_size.clone() };
													let ev = ScrcpyTouchEvent { action: _action, pointer_id: pointer_id as u64, position: pos, pressure: 0xffff, action_button: 1, buttons: 1 };//AMOTION_EVENT_BUTTON_PRIMARY
													//info!("SCRCPY Control inject event: {:?}",ev);
													let ev_bytes=ev.to_be_bytes();
													let mut payload: Vec<u8> = Vec::new();
													payload.push(ScrcpyControlMessageType::InjectTouchEvent as u8);
													payload.extend_from_slice(&ev_bytes);
													//stream.write_all(payload).await;											
													if let Err(e) = stream.write_all(&payload).await {
														error!("tsk_scrcpy_control send error: {}", e);
													}
												}
											}
											else if rsp.touchpad_event.is_some()
											{
												let touch_action = rsp.touchpad_event.action();
												for (_,touch_ev) in rsp.touchpad_event.pointer_data.iter().enumerate() {
													let touch_x = touch_ev.x();
													let touch_y = touch_ev.y();
													let pointer_id = touch_ev.pointer_id();
													let mut _action: u8;
													if touch_action == PointerAction::ACTION_DOWN
													{
														_action = AndroidTouchEvent::Down as u8;
													} else if touch_action == PointerAction::ACTION_UP
													{
														self.last_touched_point = ScrcpyPoint { x: touch_x as i32, y: touch_y as i32 };
														_action = AndroidTouchEvent::Up as u8;
													} else if touch_action == PointerAction::ACTION_MOVED
													{
														_action = AndroidTouchEvent::Move as u8;
													} else {
														error!( "tsk_scrcpy_control Received invalid touchpad action");
														continue;
													}
													let pt = ScrcpyPoint { x: touch_x as i32, y: touch_y as i32 };
													//let sz = ScrcpySize { width: video_params.res_w as u16, height: video_params.res_h as u16 };
													let pos = ScrcpyPosition { point: pt, screen_size: self.screen_size.clone() };
													let ev = ScrcpyTouchEvent { action: _action, pointer_id: pointer_id as u64, position: pos, pressure: 0xffff, action_button: 1, buttons: 1 };//AMOTION_EVENT_BUTTON_PRIMARY
													//info!("SCRCPY Control inject event: {:?}",ev);
													let ev_bytes=ev.to_be_bytes();
													let mut payload: Vec<u8> = Vec::new();
													payload.push(ScrcpyControlMessageType::InjectTouchEvent as u8);
													payload.extend_from_slice(&ev_bytes);
													//stream.write_all(payload).await;
													if let Err(e) = stream.write_all(&payload).await {
														error!("tsk_scrcpy_control send error: {}", e);
													}
												}
											}
											else if rsp.key_event.is_some()
											{
												let mut key_code=0i32;
												for (_,key_ev) in rsp.key_event.keys.iter().enumerate() {
													debug!("scrcpy_control received key_event: keycode={:?}, down={:?}",key_ev.keycode(), key_ev.down());
													let key_down = key_ev.down();
													key_code=key_ev.keycode() as i32;
													let mut _action: u8;
													if key_down
													{
														_action = AndroidKeyEvent::Down as u8;
													} else {
														_action = AndroidKeyEvent::Up as u8;
													}

													let ev = ScrcpyKeyEvent { action: _action, key_code: key_code , repeat: 0, metastate: 0 };
													//info!("SCRCPY Control inject event: {:?}",ev);
													let ev_bytes=ev.to_be_bytes();
													let mut payload: Vec<u8> = Vec::new();
													payload.push(ScrcpyControlMessageType::InjectKeycode as u8);
													payload.extend_from_slice(&ev_bytes);
													//stream.write_all(payload).await;
													if let Err(e) = stream.write_all(&payload).await {
														error!("tsk_scrcpy_control send error: {}", e);
													}
												}
											}
											else if let Some(abs_event) = rsp.absolute_event.as_ref()
											{
												for (key_ev) in &abs_event.data{
													debug!("scrcpy_control received ABS event: keycode={:?}, value={:?}",key_ev.keycode(),key_ev.value())
												}
											}
											else if let Some(rel_event) = rsp.relative_event.as_ref()
											{
												for (key_ev) in &rel_event.data {
													debug!("scrcpy_control received REL event: keycode={:?}, delta={:?}",key_ev.keycode(),key_ev.delta());
													if key_ev.keycode() == KeyCode::KEYCODE_ROTARY_CONTROLLER as u32
													{
														let ev = ScrcpyScrollEvent { position: self.last_touched_point, screen_size:self.screen_size, vscroll:key_ev.delta() as i16, hscroll:0, buttons:1 };
														//info!("SCRCPY Control inject event: {:?}",ev);
														let ev_bytes=ev.to_be_bytes();
														let mut payload: Vec<u8> = Vec::new();
														payload.push(ScrcpyControlMessageType::InjectScrollEvent as u8);
														payload.extend_from_slice(&ev_bytes);
														if let Err(e) = stream.write_all(&payload).await {
															error!("tsk_scrcpy_control send error: {}", e);
														}
													}

												}
											}
											else
											{
												error!( "tsk_scrcpy_control unmanaged key action");
											}
										}
										else
										{
											error!( "tsk_scrcpy_control: Unable to parse received message");
										}
									}
									else if message_id == ControlMessageType::MESSAGE_CUSTOM_CMD  as i32
									{
										let cmd_id: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into().unwrap()).into();
										if cmd_id == CustomCommand::CANCEL as i32
										{

											info!("CustomCommand::CANCEL cmd received, tsk_scrcpy_control task stopped");
											break;
										}
										else
										{
											error!("tsk_scrcpy_control unknown custom command received: {:?}", cmd_id);
										}
									}
									else
									{
										error!("tsk_scrcpy_control unknown message received: {:?}", message_id);
									}
								}	
								None => {
									// Sender has been dropped, exit loop
									println!("Sender closed, exiting scrcpy control loop");
									break;
								}
							}
                        }
                    }

                Ok(Err(e)) => {
                    error!("ControlServer TCP connect failed: {}", e);
                    return;
                }

                Err(_) => {
                    error!("ControlServer TCP connect timeout");
                    return;
                }
            };
            return;
        });
        ControlServerHandle { pkt_tx }
    }
}

///This task is not meant to be closed, it will always run
pub(crate) async fn tsk_adb_scrcpy(
    start_recording_servers:Arc<Notify>,
    md_connected:Arc<Notify>,
    cancel:CancellationToken,
    pconfig: SharedConfig,

) -> Result<()> {
    info!("{}: ADB task started",NAME);
    let cmd_adb = Command::new("adb").arg("start-server").output().await.unwrap();
    if !cmd_adb.status.success() {
        error!("ADB server can't start");
    }

    let mut audio_codec_params = AudioStreamingParams::default();
    let mut video_codec_params = VideoStreamingParams::default();

    let cmd_disconnect = Command::new("adb").arg("disconnect").output().await?;
    let lines=adb::parse_response_lines(cmd_disconnect.stdout).expect("TODO: panic message");
    if lines.len() > 0 {
        for line in lines {
            info!("ADB disconnect response: {:?}", line);
        }
    }
    let mut hu_conn_restart=false;
    'outer:loop
    {
        // reload new config
        let config = pconfig.read().await.clone();
        hu_conn_restart=false;
        if let Some(device)=adb::get_first_adb_device(config.clone()).await {
            info!("{}: ADB device found: {:?}, trying to get video/audio from it now",NAME, device);

            let mut cmd_portfw = vec![];
            cmd_portfw.push(format!("tcp:{}", SCRCPY_PORT));
            cmd_portfw.push("localabstract:scrcpy".to_string());
            let lines=adb::forward_cmd(cmd_portfw).await?;
            let mut port_fw_ok=true;
            if lines.len() > 0 {
                for line in lines {
                    info!("ADB port fw. response: {:?}", line);
                    if line.contains("error")
                    {
                        port_fw_ok=false;
                    }
                }
            }
            if !port_fw_ok {
                info!("ADB invalid port forward response received");
                continue;
            }
            else {
                info!("ADB port forwarding done to {}", SCRCPY_PORT);
            }

            info!("ADB config done, waiting for start server commands");
            md_connected.notify_one();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("{}: Cancel detected, starting over...",NAME);
                        continue 'outer;
                    }
                    _ = start_recording_servers.notified() => {
                        break;
                    }
                }
            }
            start_recording_servers.notified().await;
            info!("ADB config done, start server commands received");
            let video_sid=video_codec_params.sid.clone();
            let audio_sid=audio_codec_params.sid.clone();
            let mut cmd_push = vec![];
            cmd_push.push(String::from("/etc/aa-mirror-rs/scrcpy-server"));
            cmd_push.push(String::from("/data/local/tmp/scrcpy-server-manual.jar"));
            let lines=adb::push_cmd(cmd_push).await?;
            let mut push_ok=false;
            if lines.len() > 0 {
                for line in lines {
                    if line.contains("/s (")
                    {
                        push_ok=true;
                    }
                    info!("ADB push response: {:?}", line);
                }
            }
            if !push_ok {
                error!("ADB invalid push response received for control task");
                info!("tsk_adb_scrcpy Sending MD_DISCONNECT");
                /*let mut payload: Vec<u8>=Vec::new();
                payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                payload.extend_from_slice(&(CustomCommand::MD_DISCONNECTED as u16).to_be_bytes());
                let pkt_rsp = Packet {
                    channel: 0,
                    flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: std::mem::take(&mut payload),
                };
                srv_cmd_tx.send(pkt_rsp).await?;*/
                cancel.cancel();
                continue;
            }
            //Configure SCRCPY for recording
            //AVC base profile, no B frames, only I and P frames, low-latency is MANDATORY
            let video_codec_options=format!("profile:int=1,level:int=512,i-frame-interval:int={},low-latency:int=1,max-bframes:int=0",video_codec_params.fps);
            let mut cmd_shell:Vec<String> = vec![];
            let mut audio_codec="raw";
            let mut res_multiplier =1.0;
            if config.res_multiplier > 0.0f64
            {
                res_multiplier =config.res_multiplier;
            }

            if audio_codec_params.codec == MediaCodec::AUDIO_AAC_LC
            {
                audio_codec="aac";
            }
            cmd_shell.push("CLASSPATH=/data/local/tmp/scrcpy-server-manual.jar".to_string());
            cmd_shell.push("app_process".to_string());
            cmd_shell.push("/".to_string());
            cmd_shell.push("com.genymobile.scrcpy.Server".to_string());
            cmd_shell.push(SCRCPY_VERSION.to_string());
            cmd_shell.push("log_level=info".to_string());
            cmd_shell.push("raw_stream=false".to_string());//enable metadata
            cmd_shell.push("send_frame_meta=true".to_string());
            cmd_shell.push("send_stream_meta=true".to_string());
            cmd_shell.push("send_dummy_byte=false".to_string());
            cmd_shell.push("send_device_meta=false".to_string());//disable device name on video socket
            cmd_shell.push("tunnel_forward=true".to_string());
            cmd_shell.push("audio=true".to_string());
            cmd_shell.push("video=true".to_string());
            cmd_shell.push("control=true".to_string());
            cmd_shell.push("cleanup=true".to_string());
            cmd_shell.push("display_ime_policy=local".to_string());
            cmd_shell.push("stay_awake=true".to_string());
            cmd_shell.push("keep_active=true".to_string());
            cmd_shell.push(format!("audio_codec={}",audio_codec.to_string() ));
            if audio_codec_params.codec == MediaCodec::AUDIO_AAC_LC
            {
                cmd_shell.push("audio_codec_options=aac-profile:int=2".to_string());
            }
            cmd_shell.push(format!("audio_bit_rate={}", audio_codec_params.bitrate));
            cmd_shell.push(format!("max_size={}", video_codec_params.res_w));
            cmd_shell.push("video_codec=h264".to_string());
            cmd_shell.push(format!("video_codec_options={}", video_codec_options.to_string()));
            cmd_shell.push(format!("video_bit_rate={}", video_codec_params.bitrate));
            cmd_shell.push(format!("new_display={}x{}/{}", (video_codec_params.res_w as f64 * res_multiplier) as i32, (video_codec_params.res_h as f64 * res_multiplier) as i32, video_codec_params.dpi));
            cmd_shell.push(format!("max_fps={}", video_codec_params.fps));
            let (mut shell, mut sh_reader,line)=adb::shell_cmd(cmd_shell).await?;
            info!("ADB shell response: {:?}", line);
            if line.contains("[server] INFO: Device:") && shell.id().is_some()
            {
                //this waiting time is MANDATORY, otherwise we get error on video socket, why???
                tokio::time::sleep(Duration::from_millis(500)).await;//give some time to start sockets
                //wait here until something goes wrong
                loop {
                    let mut line = String::new();
                    tokio::select! {
                        result = sh_reader.read_line(&mut line) => {
                            let n = result?;

                            if n == 0 {
                                break;
                            }
                            // Process shell output
                            info!("ADB: {}", line.trim_end());
                        }

                        _ = cancel.cancelled() => {
                            info!("ADB shell Cancel received, Stopping ADB shell");

                            let _ = shell.kill().await;
                            break;
                        }
                    }
                }

            }
            else {
                error!("Invalid response for ADB shell");
                shell.kill().await?;
                continue;
            }
        }
        else {
            error!("{}: No device with ADB connection found, trying again...", NAME)
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    //Err(Box::new(stderr()))
}
