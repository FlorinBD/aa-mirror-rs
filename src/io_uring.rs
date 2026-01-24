use bytesize::ByteSize;
use humantime::format_duration;
use simplelog::*;
use std::cell::RefCell;
use std::fmt::Debug;
use std::io::stderr;
use std::io;
use std::marker::PhantomData;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::Sender as BroadcastSender;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_uring::buf::BoundedBuf;
use tokio_uring::buf::BoundedBufMut;
use tokio_uring::fs::File;
use tokio_uring::fs::OpenOptions;
use tokio_uring::net::TcpListener;
use tokio_uring::net::TcpStream;
use tokio_uring::BufResult;
use tokio_uring::UnsubmittedWrite;
use async_arp::{Client, ClientConfigBuilder, ClientSpinner, ProbeInput, ProbeInputBuilder, ProbeStatus, Result as ArpResult};
use clap::builder::TypedValueParser;
use port_check::is_port_reachable_with_timeout;
use tokio::net::ToSocketAddrs;
use crate::{adb, arp_common};
use futures::StreamExt;
use hyper::client::connect::Connect;
use tokio::process::Command;
use tokio::net::TcpStream as TokioTcpStream;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::TryRecvError;
use std::str::FromStr;
use tokio_util::bytes::BufMut;
use crate::aa_services::{VideoStreamingParams, AudioStreamingParams, CommandState, ServiceType};
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use protos::*;
use protos::ControlMessageType::{self, *};
use protobuf::{Message};
use std::cmp::min;
use serde::{Deserialize, Serialize};
use crate::h264_reader::NalReassembler;

// module name for logging engine
const NAME: &str = "<i><bright-black> io_uring: </>";

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const USB_ACCESSORY_PATH: &str = "/dev/usb_accessory";
pub const BUFFER_LEN: usize = 16 * 1024;
pub const TCP_CLIENT_TIMEOUT: Duration = Duration::new(30, 0);

use crate::config::{Action, AppConfig, SharedConfig, ADB_DEVICE_PORT, SCRCPY_VERSION, SCRCPY_PORT, SCRCPY_METADATA_HEADER_LEN};
use crate::config::{TCP_DHU_PORT, TCP_MD_SERVER_PORT};
use crate::channel_manager::{endpoint_reader, ch_proxy, packet_tls_proxy, ENCRYPTED, FRAME_TYPE_CONTROL, FRAME_TYPE_FIRST, FRAME_TYPE_LAST};
use crate::channel_manager::Packet;
use crate::config_types::HexdumpLevel;
use crate::usb_stream::{UsbStreamRead, UsbStreamWrite};

// tokio_uring::fs::File and tokio_uring::net::TcpStream are using different
// read and write calls:
// File is using read_at() and write_at(),
// TcpStream is using read() and write()
//
// In our case we are reading a special unix character device for
// the USB gadget, which is not a regular file where an offset is important.
// We just use offset 0 for reading and writing, so below is a trait
// for this, to be able to use it in a generic copy() function below.

pub trait Endpoint<E> {
    #[allow(async_fn_in_trait)]
    async fn read<T: BoundedBufMut>(&self, buf: T) -> BufResult<usize, T>;
    fn write<T: BoundedBuf>(&self, buf: T) -> UnsubmittedWrite<T>;
}

impl Endpoint<File> for File {
    async fn read<T: BoundedBufMut>(&self, buf: T) -> BufResult<usize, T> {
        self.read_at(buf, 0).await
    }
    fn write<T: BoundedBuf>(&self, buf: T) -> UnsubmittedWrite<T> {
        self.write_at(buf, 0)
    }
}

impl Endpoint<TcpStream> for TcpStream {
    async fn read<T: BoundedBufMut>(&self, buf: T) -> BufResult<usize, T> {
        self.read(buf).await
    }
    fn write<T: BoundedBuf>(&self, buf: T) -> UnsubmittedWrite<T> {
        self.write(buf)
    }
}

pub enum IoDevice<A: Endpoint<A>> {
    UsbReader(Rc<RefCell<UsbStreamRead>>, PhantomData<A>),
    UsbWriter(Rc<RefCell<UsbStreamWrite>>, PhantomData<A>),
    EndpointIo(Rc<A>),
    TcpStreamIo(Rc<TcpStream>),
}
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
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpyTouchEvent {
    pub action: u8,
    pub pointer_id: u64,
    pub position:ScrcpyPosition,
    pub pressure:i16,
    pub action_button:i32,
    pub buttons:i32,
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpyKeyEvent {
    pub action: u8,
    pub key_code: i32,
    pub repeat:i32,
    pub metastate:i32,
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpyPosition {
    pub point: ScrcpyPoint,
    pub screen_size: ScrcpySize,
}

#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpyPoint {
    pub x: i32,
    pub y: i32,
}
#[derive(Serialize, Deserialize, Copy, Clone, Debug, Default)]
pub struct ScrcpySize {
    pub width: i32,
    pub height: i32,
}

async fn transfer_monitor(
    stats_interval: Option<Duration>,
    usb_bytes_written: Arc<AtomicUsize>,
    tcp_bytes_written: Arc<AtomicUsize>,
    read_timeout: Duration,
    config: SharedConfig,
) -> Result<()> {
    let mut usb_bytes_out_last: usize = 0;
    let mut tcp_bytes_out_last: usize = 0;
    let mut stall_usb_bytes_last: usize = 0;
    let mut stall_tcp_bytes_last: usize = 0;
    let mut report_time = Instant::now();
    let mut stall_check = Instant::now();

    info!(
        "{} ⚙️ Showing transfer statistics: <b><blue>{}</>",
        NAME,
        match stats_interval {
            Some(d) => format_duration(d).to_string(),
            None => "disabled".to_string(),
        }
    );

    loop {
        // load current total transfer from AtomicUsize:
        let usb_bytes_out = usb_bytes_written.load(Ordering::Relaxed);
        let tcp_bytes_out = tcp_bytes_written.load(Ordering::Relaxed);

        // Stats printing
        if stats_interval.is_some() && report_time.elapsed() > stats_interval.unwrap() {
            // compute USB transfer
            usb_bytes_out_last = usb_bytes_out - usb_bytes_out_last;
            let usb_transferred_total = ByteSize::b(usb_bytes_out.try_into().unwrap());
            let usb_transferred_last = ByteSize::b(usb_bytes_out_last.try_into().unwrap());
            let usb_speed: u64 =
                (usb_bytes_out_last as f64 / report_time.elapsed().as_secs_f64()).round() as u64;
            let usb_speed = ByteSize::b(usb_speed);

            // compute TCP transfer
            tcp_bytes_out_last = tcp_bytes_out - tcp_bytes_out_last;
            let tcp_transferred_total = ByteSize::b(tcp_bytes_out.try_into().unwrap());
            let tcp_transferred_last = ByteSize::b(tcp_bytes_out_last.try_into().unwrap());
            let tcp_speed: u64 =
                (tcp_bytes_out_last as f64 / report_time.elapsed().as_secs_f64()).round() as u64;
            let tcp_speed = ByteSize::b(tcp_speed);

            info!(
                "{} {} {: >9} ({: >9}/s), {: >9} total | {} {: >9} ({: >9}/s), {: >9} total",
                NAME,
                "phone -> car 🔺",
                usb_transferred_last.to_string_as(true),
                usb_speed.to_string_as(true),
                usb_transferred_total.to_string_as(true),
                "car -> phone 🔻",
                tcp_transferred_last.to_string_as(true),
                tcp_speed.to_string_as(true),
                tcp_transferred_total.to_string_as(true),
            );

            // save values for next iteration
            report_time = Instant::now();
            usb_bytes_out_last = usb_bytes_out;
            tcp_bytes_out_last = tcp_bytes_out;
        }

        // transfer stall detection
        if stall_check.elapsed() > read_timeout {
            // compute delta since last check
            stall_usb_bytes_last = usb_bytes_out - stall_usb_bytes_last;
            stall_tcp_bytes_last = tcp_bytes_out - stall_tcp_bytes_last;

            if stall_usb_bytes_last == 0 || stall_tcp_bytes_last == 0 {
                return Err("unexpected transfer stall".into());
            }

            // save values for next iteration
            stall_check = Instant::now();
            stall_usb_bytes_last = usb_bytes_out;
            stall_tcp_bytes_last = tcp_bytes_out;
        }

        // check pending action
        let action = config.read().await.action_requested.clone();
        if let Some(action) = action {
            // check if we need to restart or reboot
            if action == Action::Reconnect {
                config.write().await.action_requested = None;
            }
            return Err(format!("action request: {:?}", action).into());
        }

        sleep(Duration::from_millis(100)).await;
    }
}

async fn flatten<T>(handle: &mut JoinHandle<Result<T>>, dbg_info:String) -> Result<T> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(er) => Err(format!("task handling failed for {}", dbg_info).into()),
    }
}

/// Asynchronously wait for an inbound TCP connection
/// returning TcpStream of first client connected
async fn tcp_wait_for_hu_connection_old(listener: & TcpListener) -> Result<TcpStream> {
    let retval = listener.accept();
    let (stream, addr) = match timeout(TCP_CLIENT_TIMEOUT, retval)
        .await
        .map_err(|e| std::io::Error::other(e))
    {
        Ok(Ok((stream, addr))) => (stream, addr),
        Err(e) | Ok(Err(e)) => {
            error!("{} 📵 HU TCP server: {}, restarting...", NAME, e);
            return Err(Box::new(e));
        }
    };
    info!(
        "{} 📳 HU TCP server: new client connected: <b>{:?}</b>",
        NAME, addr
    );

    // disable Nagle algorithm, so segments are always sent as soon as possible,
    // even if there is only a small amount of data
    stream.set_nodelay(true)?;
    Ok(stream)
}

async fn tcp_wait_for_hu_connection_old2(listener: & TcpListener) -> Result<TcpStream> {
    tokio::select! {
        accept_result = listener.accept() => {
            match accept_result {
                Ok((stream, addr)) => {
                    info!("HU TCP Client connected: {:?}", addr);
                    stream.set_nodelay(true)?;
                    Ok(stream)
                }
                Err(e) => {
                    error!("{} 📵 HU TCP server error: {}, restarting...", NAME, e);
                    Err(Box::new(e))
                }
            }
        }
        _ = sleep(TCP_CLIENT_TIMEOUT) => {
            error!("{} 📵 HU TCP server timeout elapsed, restarting...", NAME);
            Err("Timeout waiting for client".into())
        }
    }
}

async fn tcp_wait_for_hu_connection(listener: & TcpListener) -> Result<TcpStream> {
    // Accept one client
    let (stream, addr) = listener.accept().await?;
    println!("DHU Client connected: {:?}", addr);

    // Disable Nagle algorithm if you want low-latency small packets
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Asynchronously wait for an inbound TCP connection
/// returning TcpStream of first client connected
async fn tcp_wait_for_md_connection(listener: &mut TcpListener) -> Result<TcpStream> {
    let retval = listener.accept();
    let (stream, addr) = match timeout(TCP_CLIENT_TIMEOUT, retval)
        .await
        .map_err(|e| std::io::Error::other(e))
    {
        Ok(Ok((stream, addr))) => (stream, addr),
        Err(e) | Ok(Err(e)) => {
            error!("{} 📵 MD TCP server: {}, restarting...", NAME, e);
            return Err(Box::new(e));
        }
    };
    info!(
        "{} 📳 MD TCP server: new client connected: <b>{:?}</b>",
        NAME, addr
    );

    // disable Nagle algorithm, so segments are always sent as soon as possible,
    // even if there is only a small amount of data
    stream.set_nodelay(true)?;
    Ok(stream)
}


async fn tsk_scrcpy_video(
    mut stream: TcpStream,
    mut cmd_rx: flume::Receiver<Packet>,
    video_tx: flume::Sender<Packet>,
    max_unack:u32,
    sid:u8,
) -> Result<()> {
    info!("Starting video server!");
    let mut streaming_on=true;
    let mut act_unack=0;
    //let mut frame_buf = vec![];
    let codec_buf = vec![0u8; 12];
    //let header_buf = vec![0u8; 12];
    let mut i=0;
    //let mut ch_id:u8=0;

    //discard codec metadata
    let (res, buf_out) = stream.read(codec_buf).await;
    let n = res?;
    if n == 0 {
        error!("Video connection closed by server?");
        return Err(Box::new(io::Error::new(
            io::ErrorKind::Other,
            "Video connection closed by server",
        )));
    }
    if n != 12 {
        error!("Video codec reading error");
        return Err(Box::new(io::Error::new(
            io::ErrorKind::Other,
            "Video codec reading error",
        )));
    }
    info!("SCRCPY Video codec metadata: {:02x?}", &buf_out[..n]);
    let mut codec_id=String::from_utf8_lossy(&buf_out[0..4]).to_string();
        codec_id=codec_id.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    //let codec_id = u32::from_be_bytes(buf_out[0..4].try_into().unwrap());
    let video_res_w = u32::from_be_bytes(buf_out[4..8].try_into().unwrap());
    let video_res_h = u32::from_be_bytes(buf_out[8..12].try_into().unwrap());
    info!("SCRCPY Video metadata decoded: id={}, res_w={}, res_h={}", codec_id, video_res_w, video_res_h);
    if (codec_id != "h264".to_string()) || (video_res_w != 800) || (video_res_h != 480) {
        error!("SCRCPY Invalid Video codec configuration");
        return Err(Box::new(io::Error::new(io::ErrorKind::Other, "SCRCPY Invalid Video codec configuration")));
    }
    info!("SCRCPY Video entering main loop");
    let timestamp: u64 = 0;//is not used by HU
    let mut payload: Vec<u8>=Vec::new();
    //let mut reassembler = NalReassembler::new();
    loop {
        tokio::select! {
            biased;
            //Read encapsulated video frames, we don't have to slice NAL units, we can send all together as they come from MediaEncoder
            res = read_scrcpy_packet(&mut stream) => {
                    let (pts, h264_data) = res?;
                    //let nals = reassembler.feed(&h264_data);
                    let key_frame=(pts & 0x4000_0000_0000_0000u64) >0;
                    let rec_ts=pts & 0x3FFF_FFFF_FFFF_FFFFu64;
                    let config_frame=(pts & 0x8000_0000_0000_0000u64) >0;
                    let rd_len=h264_data.len();
                    let dbg_len=min(rd_len, 16);
                    if i<10
                    {
                        if rd_len>dbg_len
                        {
                            let end_offset= rd_len - dbg_len;
                            info!("Video task got frame config={:?}, act size: {}, raw slice: {:02x?}...{:02x?}",config_frame, rd_len, &h264_data[..dbg_len], &h264_data[end_offset..]);
                        }
                        else
                        {
                            info!("Video task got frame config={:?}, act size: {}, raw bytes: {:02x?}",config_frame, rd_len, &h264_data[..dbg_len]);
                        }

                    i=i+1;
                    }

                    if streaming_on && (act_unack < max_unack)
                    {

                        payload.clear();
                        if config_frame
                        {
                            payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16).to_be_bytes());
                            payload.extend_from_slice(&h264_data);
                        }
                        else {
                            payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_DATA as u16).to_be_bytes());
                            payload.extend_from_slice(&timestamp.to_be_bytes());
                            payload.extend_from_slice(&h264_data);
                        }

                        let pkt_rsp = Packet {
                            channel: sid,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: std::mem::take(&mut payload),
                        };
                        video_tx.send_async(pkt_rsp).await?;
                        act_unack=act_unack+1;
                    }
                }
            Ok(pkt) = cmd_rx.recv_async() =>
                {
                    //info!("tsk_scrcpy_video Received command packet {:02x?}", pkt);
                    let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
                    if message_id == MESSAGE_CUSTOM_CMD  as i32
                    {
                        let cmd_id: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                        let data = &pkt.payload[4..]; // start of message data, without message_id
                        if cmd_id == CustomCommand::CMD_STOP_VIDEO_RECORDING as i32
                        {
                            act_unack=max_unack;
                            streaming_on = false;
                            info!("tsk_scrcpy_video Video streaming stopped");
                            break;
                        }

                    }
                    else if message_id == MediaMessageId::MEDIA_MESSAGE_ACK  as i32
                    {
                        if pkt.channel ==  sid
                        {
                            //info!("{} Received {} message", sid.to_string(), message_id);
                            let data = &pkt.payload[2..]; // start of message data, without message_id
                            if  let Ok(rsp) = Ack::parse_from_bytes(&data)
                            {
                                //info!( "{}, channel {:?}: ACK, timestamp_ns: {:?}", get_name(), pkt.channel, rsp.receive_timestamp_ns[0]);
                                info!( "tsk_scrcpy_video: video ACK received, sending next frame");
                                act_unack=0;
                            }
                            else
                            {
                                error!( "tsk_scrcpy_video Unable to parse received message");
                            }
                        }
                        else {
                        //info!( "tsk_scrcpy_video: media ACK received but not for VIDEO, discarded");
                        }
                    }
                    else
                    {
                        error!("tsk_scrcpy_video unknown message id: {}", message_id);
                    }
                }
            else => {
                // all branches are closed
                error!("All SCRCPY Video tasks are closed");
                break;
            }
        }
    }
    //reassembler.flush();
    return Ok(());
    async fn read_exact(
        stream: &mut TcpStream,
        size: usize,
    ) -> io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(size);

        while buf.len() < size {
            let to_read = size - buf.len();
            let chunk = vec![0u8; to_read];

            let (res, chunk) = stream.read(chunk).await;
            let rd=res?;

            if rd == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
            }

            buf.extend_from_slice(&chunk[..rd]);
        }

        Ok(buf)
    }
    /// Read one scrcpy packet (with metadata)
    async fn read_scrcpy_packet(stream: &mut TcpStream) -> io::Result<(u64, Vec<u8>)> {
        // First 12 bytes = packet metadata
        let metadata=read_exact(stream, SCRCPY_METADATA_HEADER_LEN).await?;
        if metadata.len() != SCRCPY_METADATA_HEADER_LEN {
            error!("read_scrcpy_packet data len error, wanted {} but got {} bytes", SCRCPY_METADATA_HEADER_LEN, metadata.len());
        }
        let packet_size = u32::from_be_bytes(metadata[8..].try_into().unwrap()) as usize;
        let pts = u64::from_be_bytes(metadata[0..8].try_into().unwrap());
        // Read full packet
        let payload=read_exact(stream, packet_size).await?;

        let h264_data = payload.to_vec();
        if h264_data.len() != packet_size {
            error!("read_scrcpy_packet data len error, wanted {} but got {} bytes", packet_size, h264_data.len());
        }
        Ok((pts, h264_data))
    }
}

async fn tsk_scrcpy_audio(
    mut stream: TcpStream,
    mut cmd_rx: flume::Receiver<Packet>,
    audio_tx: flume::Sender<Packet>,
    max_unack:u32,
    sid:u8
) -> Result<()> {

    info!("Starting audio server!");
    let mut streaming_on=false;
    //let mut ch_id:u8=0;
    let mut act_unack=0;
    //discard codec metadata
    let codec_buf = vec![0u8; 4];
    let (res, buf_out) = stream.read(codec_buf).await;
    let n = res?;
    if n == 0 {
        error!("Audio connection closed by server?");
        return Err(Box::new(io::Error::new(
            io::ErrorKind::Other,
            "Audio connection closed by server?",
        )));
    }
    if n != 4 {
        error!("Audio codec reading error");
        return Err(Box::new(io::Error::new(
            io::ErrorKind::Other,
            "Audio codec reading error",
        )));
    }
    info!("SCRCPY Audio codec metadata: {:02x?}", &buf_out[..n]);
    let mut codec_id=String::from_utf8_lossy(&buf_out[0..4]).to_string();
        codec_id=codec_id.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    info!("SCRCPY Audio codec id: {}", codec_id);
    if codec_id != "aac".to_string() {
        error!("SCRCPY Invalid audio codec configuration");
        return Err(Box::new(io::Error::new(io::ErrorKind::Other, "SCRCPY Invalid audio codec configuration")));
    }
    let mut buf = vec![0u8; 0xffff];
    let mut i=0;
    let timestamp: u64 = 0;//is not used by HU
    loop {
        tokio::select! {
            biased;
            res = read_scrcpy_packet(&mut stream) => {
                let (pts, data) = res?;
                let rd_len=data.len();
                if rd_len == 0 {
                    info!("Audio connection closed by server?");
                    return Err(Box::new(io::Error::new(
                        io::ErrorKind::Other,
                        "Audio connection closed by server",
                        )));
                }
                let dbg_len=min(rd_len,16);
                if i<5
                {
                    info!("Audio task Read {} bytes: {:02x?}", n, &buf_out[..dbg_len]);
                    i=i+1;
                }
                if streaming_on && (act_unack<max_unack)
                {
                    let mut payload: Vec<u8>=Vec::new();
                    payload.extend_from_slice(&timestamp.to_be_bytes());
                    payload.extend_from_slice(&buf_out[12..n]);
                    payload.insert(0, ((MediaMessageId::MEDIA_MESSAGE_DATA as u16) >> 8) as u8);
                    payload.insert(1, ((MediaMessageId::MEDIA_MESSAGE_DATA as u16) & 0xff) as u8);

                    let pkt_rsp = Packet {
                        channel: sid,
                        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                        final_length: None,
                        payload: payload,
                    };
                    if let Err(_) = audio_tx.send(pkt_rsp){
                        error!( "SCRCPY audio frame send error");
                    };
                }
            }
            Ok(pkt) = cmd_rx.recv_async() =>
            {
                    //info!("tsk_scrcpy_audio Received command packet {:02x?}", pkt);
                let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
                if message_id == MESSAGE_CUSTOM_CMD  as i32
                {
                    let cmd_id: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                    //let data = &pkt.payload[4..]; // start of message data, without message_id
                    if cmd_id == CustomCommand::CMD_STOP_AUDIO_RECORDING as i32
                    {
                        act_unack=max_unack;
                        streaming_on = false;
                        info!("tsk_scrcpy_audio Audio streaming stopped");
                    }

                }
                else if message_id == MediaMessageId::MEDIA_MESSAGE_ACK  as i32
                {
                    if pkt.channel == sid
                    {
                        info!("{} Received {} message", sid.to_string(), message_id);
                        let data = &pkt.payload[2..]; // start of message data, without message_id
                        if  let Ok(rsp) = Ack::parse_from_bytes(&data)
                        {
                            //info!( "{}, channel {:?}: ACK, timestamp_ns: {:?}", get_name(), pkt.channel, rsp.receive_timestamp_ns[0]);
                            info!( "tsk_scrcpy_audio: media ACK received, sending next frame");
                            act_unack=0;
                        }
                        else
                        {
                            error!( "tsk_scrcpy_audio Unable to parse MEDIA ACK message");
                        }
                    }
                    else {
                        //info!( "tsk_scrcpy_audio: media ACK received but not for AUDIO, discarded");
                    }
                }
                else
                {
                    error!("tsk_scrcpy_video unknown message id: {}", message_id);
                }
            }
            else => {
                // all branches are closed
                error!("All SCRCPY Audio tasks are closed");
                break;
            }
        }
    }
    return Ok(());

    async fn read_exact(
        stream: &mut TcpStream,
        size: usize,
    ) -> io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(size);

        while buf.len() < size {
            let to_read = size - buf.len();
            let chunk = vec![0u8; to_read];

            let (res, chunk) = stream.read(chunk).await;
            let rd=res?;

            if rd == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
            }

            buf.extend_from_slice(&chunk[..rd]);
        }

        Ok(buf)
    }
    /// Read one scrcpy packet (with metadata)
    async fn read_scrcpy_packet(stream: &mut TcpStream) -> io::Result<(u64, Vec<u8>)> {
        // First 12 bytes = packet metadata
        let metadata=read_exact(stream, SCRCPY_METADATA_HEADER_LEN).await?;
        if metadata.len() != SCRCPY_METADATA_HEADER_LEN {
            error!("read_scrcpy_packet data len error, wanted {} but got {} bytes", SCRCPY_METADATA_HEADER_LEN, metadata.len());
        }
        let packet_size = u32::from_be_bytes(metadata[8..].try_into().unwrap()) as usize;
        let pts = u64::from_be_bytes(metadata[0..8].try_into().unwrap());
        // Read full packet
        let payload=read_exact(stream, packet_size).await?;

        let h264_data = payload.to_vec();
        if h264_data.len() != packet_size {
            error!("read_scrcpy_packet data len error, wanted {} but got {} bytes", packet_size, h264_data.len());
        }
        Ok((pts, h264_data))
    }
}

async fn tsk_scrcpy_control(
    mut stream: TcpStream,
    cmd_rx: flume::Receiver<Packet>,
    video_params:VideoStreamingParams,
) -> Result<()> {
    info!("Starting control server!");
    loop {
        match cmd_rx.recv_async().await {
            Ok(pkt) => {
                //FIXME drop postcard and use BytesMut to have BE serialization
                continue;
                // Received a packet
                let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
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
                                } else if touch_action == PointerAction::ACTION_MOVED
                                {
                                    _action = AndroidTouchEvent::Move as u8;
                                } else {
                                    error!( "tsk_scrcpy_control Received invalid touchscreen action");
                                    continue;
                                }
                                let pt = ScrcpyPoint { x: touch_x as i32, y: touch_y as i32 };
                                let sz = ScrcpySize { width: video_params.res_h, height: video_params.res_h };
                                let pos = ScrcpyPosition { point: pt, screen_size: sz };
                                let ev = ScrcpyTouchEvent { action: _action, pointer_id: pointer_id as u64, position: pos, pressure: 255, action_button: 0, buttons: 0 };
                                info!("SCRCPY Control inject event: {:?}",ev);
                                let ev_bytes: Vec<u8> = postcard::to_stdvec(&ev)?;
                                let mut payload: Vec<u8> = Vec::new();
                                payload.push(ScrcpyControlMessageType::InjectTouchEvent as u8);
                                payload.extend_from_slice(&ev_bytes);
                                stream.write_all(payload).await;
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
                                    _action = AndroidTouchEvent::Up as u8;
                                } else if touch_action == PointerAction::ACTION_MOVED
                                {
                                    _action = AndroidTouchEvent::Move as u8;
                                } else {
                                    error!( "tsk_scrcpy_control Received invalid touchpad action");
                                    continue;
                                }
                                let pt = ScrcpyPoint { x: touch_x as i32, y: touch_y as i32 };
                                let sz = ScrcpySize { width: video_params.res_h, height: video_params.res_h };
                                let pos = ScrcpyPosition { point: pt, screen_size: sz };
                                let ev = ScrcpyTouchEvent { action: _action, pointer_id: pointer_id as u64, position: pos, pressure: 255, action_button: 0, buttons: 0 };
                                info!("SCRCPY Control inject event: {:?}",ev);
                                let ev_bytes: Vec<u8> = postcard::to_stdvec(&ev)?;
                                let mut payload: Vec<u8> = Vec::new();
                                payload.push(ScrcpyControlMessageType::InjectTouchEvent as u8);
                                payload.extend_from_slice(&ev_bytes);
                                stream.write_all(payload).await;
                            }
                        }
                        else if rsp.key_event.is_some()
                        {
                            for (_,key_ev) in rsp.key_event.keys.iter().enumerate() {
                                let key_down = key_ev.down();
                                let mut _action: u8;
                                if key_down
                                {
                                    _action = AndroidKeyEvent::Down as u8;
                                } else {
                                    _action = AndroidKeyEvent::Up as u8;
                                }

                                let ev = ScrcpyKeyEvent { action: _action, key_code: key_ev.keycode() as i32, repeat: 0, metastate: 0 };
                                info!("SCRCPY Control inject event: {:?}",ev);
                                let ev_bytes: Vec<u8> = postcard::to_stdvec(&ev)?;
                                let mut payload: Vec<u8> = Vec::new();
                                payload.push(ScrcpyControlMessageType::InjectKeycode as u8);
                                payload.extend_from_slice(&ev_bytes);
                                stream.write_all(payload).await;
                            }
                        }
                        else {
                            error!( "tsk_scrcpy_control unmanaged key action");
                        }
                    }
                    else {
                        error!( "tsk_scrcpy_control: Unable to parse received message");
                    }

                }
                else {
                    error!("tsk_scrcpy_control unknown message received: {:?}", message_id);
                }
            }
            Err(flume::RecvError::Disconnected) => {
                // Sender has been dropped, exit loop
                println!("Sender closed, exiting scrcpy control loop");
                break;
            }
        }
    }
    Err(Box::new(flume::RecvError::Disconnected))
}
async fn tsk_adb_scrcpy(
    media_tx: flume::Sender<Packet>,
    mut srv_cmd_rx_scrcpy: flume::Receiver<Packet>,
    srv_cmd_tx: flume::Sender<Packet>,
    rx_scrcpy_ctrl: flume::Receiver<Packet>,
    config: AppConfig,
) -> Result<()> {
    info!("{}: ADB task started",NAME);
    let cmd_adb = Command::new("adb")
        .arg("start-server")
        .output().await.unwrap();
    if !cmd_adb.status.success() {
        error!("ADB server can't start");
    }

    let mut audio_codec_params = AudioStreamingParams::default();
    let mut video_codec_params = VideoStreamingParams::default();

    let cmd_disconnect = Command::new("adb")
        .arg("disconnect")
        .output().await?;
    let lines=adb::parse_response_lines(cmd_disconnect.stdout).expect("TODO: panic message");
    if lines.len() > 0 {
        for line in lines {
            info!("ADB disconnect response: {:?}", line);
        }
    }
    loop
    {
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
            info!("ADB config done, sending MD_CONNECTED and wait for start recording");
            let mut payload: Vec<u8>=Vec::new();
            payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
            payload.extend_from_slice(&(CustomCommand::MD_CONNECTED as u16).to_be_bytes());
            let pkt_rsp = Packet {
                channel: 0,
                flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                final_length: None,
                payload: std::mem::take(&mut payload),
            };
            srv_cmd_tx.send_async(pkt_rsp).await?;
            //wait for custom CMD to start recording
            loop {
                match srv_cmd_rx_scrcpy.recv_async().await {
                    Ok(pkt) => {
                        // Received a packet
                        info!("tsk_adb_scrcpy Received command packet {:02x?}", pkt);
                        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
                        if message_id == MESSAGE_CUSTOM_CMD  as i32
                        {
                            let cmd_id: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                            let data = &pkt.payload[4..]; // start of message data, without message_id
                            if cmd_id == CustomCommand::CMD_START_VIDEO_RECORDING as i32
                            {
                                match postcard::take_from_bytes::<VideoStreamingParams>(data) {
                                    Ok((cmd, rest)) => {
                                        info!("Parsed VideoStreamingParams: {:?}", cmd);
                                        info!("Remaining bytes: {}", rest.len());
                                        video_codec_params =cmd;
                                        break;
                                    }
                                    Err(e) => {
                                        error!("postcard parsing error: {:?}", e);
                                    }
                                }
                            }
                            else if cmd_id == CustomCommand::CMD_START_AUDIO_RECORDING as i32
                            {
                                match postcard::take_from_bytes::<AudioStreamingParams>(data) {
                                    Ok((cmd, rest)) => {
                                        info!("Parsed AudioStreamingParams: {:?}", cmd);
                                        info!("Remaining bytes: {}", rest.len());
                                        audio_codec_params =cmd;
                                        //break; FIXME start recording on audio also
                                    }
                                    Err(e) => {
                                        error!("postcard parsing error: {:?}", e);
                                    }
                                }
                            }
                            else
                            {
                                info!("tsk_adb_scrcpy unknown command received");
                            }

                        }
                        else {
                            error!("tsk_adb_scrcpy unknown message received: {:?}", message_id);
                        }
                    }
                    Err(flume::RecvError::Disconnected) => {
                        // Sender has been dropped, exit loop
                        println!("Sender closed, exiting loop");
                    }
                }
            }

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
                let mut payload: Vec<u8>=Vec::new();
                payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                payload.extend_from_slice(&(CustomCommand::MD_DISCONNECTED as u16).to_be_bytes());
                let pkt_rsp = Packet {
                    channel: 0,
                    flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: std::mem::take(&mut payload),
                };
                srv_cmd_tx.send_async(pkt_rsp).await?;
                continue;
            }
            //AVC base profile, no B frames, only I and P frames, low-latency is MANDATORY
            let video_codec_options=format!("profile:int=1,level:int=512,i-frame-interval:int={},low-latency:int=1,max-bframes:int=0",video_codec_params.fps);
            let mut cmd_shell:Vec<String> = vec![];
            cmd_shell.push("CLASSPATH=/data/local/tmp/scrcpy-server-manual.jar".to_string());
            cmd_shell.push("app_process".to_string());
            cmd_shell.push("/".to_string());
            cmd_shell.push("com.genymobile.scrcpy.Server".to_string());
            cmd_shell.push(SCRCPY_VERSION.to_string());
            cmd_shell.push("log_level=info".to_string());
            cmd_shell.push("raw_stream=false".to_string());//enable metadata
            cmd_shell.push("send_frame_meta=true".to_string());
            cmd_shell.push("send_codec_meta=true".to_string());
            cmd_shell.push("send_dummy_byte=false".to_string());
            cmd_shell.push("send_device_meta=false".to_string());//disable device name on video socket
            cmd_shell.push("tunnel_forward=true".to_string());
            cmd_shell.push("audio=true".to_string());
            cmd_shell.push("video=true".to_string());
            cmd_shell.push("control=true".to_string());
            cmd_shell.push("cleanup=true".to_string());
            cmd_shell.push("audio_codec=aac".to_string());
            cmd_shell.push(format!("audio_bit_rate={}", audio_codec_params.bitrate));
            cmd_shell.push(format!("max_size={}", video_codec_params.res_w));
            cmd_shell.push("video_codec=h264".to_string());
            cmd_shell.push(format!("video_codec_options={}", video_codec_options.to_string()));
            cmd_shell.push(format!("video_bit_rate={}", video_codec_params.bitrate));
            cmd_shell.push(format!("new_display={}x{}/{}", video_codec_params.res_w, video_codec_params.res_h, video_codec_params.dpi));
            cmd_shell.push(format!("max_fps={}", video_codec_params.fps));
            let (mut shell, mut sh_reader,line)=adb::shell_cmd(cmd_shell).await?;
            info!("ADB video shell response: {:?}", line);
            if line.contains("[server] INFO: Device:") && shell.id().is_some()
            {
                tokio::time::sleep(Duration::from_secs(3)).await;//give some time to start sockets
                let addr = format!("127.0.0.1:{}", SCRCPY_PORT).parse().unwrap();
                let mut video_stream = TcpStream::connect(addr).await?;
                video_stream.set_nodelay(true)?;
                let addr = format!("127.0.0.1:{}", SCRCPY_PORT).parse().unwrap();
                let mut audio_stream = TcpStream::connect(addr).await?;
                audio_stream.set_nodelay(true)?;
                let addr = format!("127.0.0.1:{}", SCRCPY_PORT).parse().unwrap();
                let mut ctrl_stream = TcpStream::connect(addr).await?;
                ctrl_stream.set_nodelay(true)?;
                info!("SCRCPY connected to all 3 sockets");
                let hnd_scrcpy_video;
                let hnd_scrcpy_audio;
                let hnd_scrcpy_ctrl;
                let video_tx = media_tx.clone();
                let audio_tx = media_tx.clone();
                let (done_th_tx_video, mut done_th_rx_video) = oneshot::channel();
                let (done_th_tx_audio, mut done_th_rx_audio) = oneshot::channel();
                let (done_th_tx_ctrl, mut done_th_rx_ctrl) = oneshot::channel();
                let rx_cmd_video = srv_cmd_rx_scrcpy.clone();
                let rx_cmd_audio = srv_cmd_rx_scrcpy.clone();
                let rx_cmd_ctrl = rx_scrcpy_ctrl.clone();

                hnd_scrcpy_video = tokio_uring::spawn(async move {
                    let res = tsk_scrcpy_video(
                        video_stream,
                        rx_cmd_video,
                        video_tx,
                        video_codec_params.max_unack,
                        video_codec_params.sid).await;
                    let _ = done_th_tx_video.send(res);

                });
                hnd_scrcpy_audio = tokio_uring::spawn(async move {
                    let res = tsk_scrcpy_audio(
                        audio_stream,
                        rx_cmd_audio,
                        audio_tx,
                        audio_codec_params.max_unack,
                        audio_codec_params.sid,
                    ).await;
                    let _ = done_th_tx_audio.send(res);
                });

                hnd_scrcpy_ctrl = tokio_uring::spawn(async move {
                    let res = tsk_scrcpy_control(
                        ctrl_stream,
                        rx_cmd_ctrl,
                        video_codec_params.clone(),
                    ).await;
                    let _ = done_th_tx_ctrl.send(res);
                });


                info!("Connected to control server!");
                let mut buf = [0u8; 1024];
                loop {
                    tokio::select! {
                    biased;
                        Ok(n) = sh_reader.read(&mut buf) => {
                            info!("shell stdout: {}", String::from_utf8_lossy(&buf[..n]));
                        }
                        Ok(..) = &mut done_th_rx_video => {
                            error!("SCRCPY Video Task finished");
                            break;
                        }
                        Ok(..) = &mut done_th_rx_audio => {
                            error!("SCRCPY Audio Task finished");
                            break;
                        }
                        Ok(..) = &mut done_th_rx_ctrl => {
                            error!("SCRCPY Control Task finished");
                            break;
                        }
                        else => {
                            // all branches are closed
                            error!("All SCRCPY tasks are closed");
                            break;
                        }
                    }

                }
                // When done, stop the shell
                shell.kill().await?;
            }
            else {
                error!("Invalid response for ADB shell");
                info!("tsk_adb_scrcpy Sending MD_DISCONNECT");
                let mut payload: Vec<u8>=Vec::new();
                payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                payload.extend_from_slice(&(CustomCommand::MD_DISCONNECTED as u16).to_be_bytes());
                let pkt_rsp = Packet {
                    channel: 0,
                    flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: std::mem::take(&mut payload),
                };
                srv_cmd_tx.send_async(pkt_rsp).await?;
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

pub async fn io_loop(
    need_restart: BroadcastSender<Option<Action>>,
    config: SharedConfig,
    tx: Arc<Mutex<Option<Sender<Packet>>>>,
) -> Result<()> {
    let shared_config = config.clone();
    #[allow(unused_variables)]
    //let (client_handler, ev_tx) = spawn_ev_client_task().await;

    // prepare/bind needed TCP listeners
    /*info!("{} 🛰️ Starting TCP server for MD...", NAME);
    let bind_addr = format!("0.0.0.0:{}", TCP_SERVER_PORT).parse().unwrap();
    let mut md_listener = Some(TcpListener::bind(bind_addr).unwrap());
    info!("{} 🛰️ MD TCP server bound to: <u>{}</u>", NAME, bind_addr);*/
    info!("{} 🛰️ Starting TCP server for DHU...", NAME);
    let bind_addr = format!("0.0.0.0:{}", TCP_DHU_PORT).parse().unwrap();
    let mut dhu_listener = Some(TcpListener::bind(bind_addr).unwrap());
    info!("{} 🛰️ DHU TCP server bound to: <u>{}</u>", NAME, bind_addr);
    let cfg = shared_config.read().await.clone();
    let hex_requested = cfg.hexdump_level;

    //io channels for scrcpy
    //media frames channel, scrcpy>HU, TODO implement Arc<Packet> to solve copy
    let (tx_scrcpy, rx_scrcpy)=flume::bounded::<Packet>(60);
    //cmd srv>scrcpy channel
    let (tx_scrcpy_cmd, rx_scrcpy_cmd)=flume::bounded::<Packet>(5);
    //cmd scrcpy>srv channel
    let (tx_scrcpy_srv_cmd, rx_scrcpy_srv_cmd)=flume::bounded::<Packet>(5);
    //scrcpy control channel HU>MD
    let (tx_scrcpy_control, rx_scrcpy_control)=flume::bounded::<Packet>(5);

    let mut tsk_adb;
    tsk_adb = tokio_uring::spawn(tsk_adb_scrcpy(
        tx_scrcpy,
        rx_scrcpy_cmd,
        tx_scrcpy_srv_cmd,
        rx_scrcpy_control,
        cfg,
    ));
    loop {
        // reload new config
        let config = config.read().await.clone();

        // generate Durations from configured seconds
        let stats_interval = {
            if config.stats_interval == 0 {
                None
            } else {
                Some(Duration::from_secs(config.stats_interval.into()))
            }
        };
        let read_timeout = Duration::from_secs(config.timeout_secs.into());

        let mut hu_tcp = None;
        let mut hu_usb = None;
        //let mut dhu_listener ;
        if config.dhu {
            //info!("{} 🛰️ DHU TCP server: bind to local address",NAME);
            //dhu_listener = Some(TcpListener::bind(bind_addr).unwrap());
            info!("{} 🛰️ DHU TCP server: listening for `Desktop Head Unit` connection...",NAME);
            if let Ok(s) = tcp_wait_for_hu_connection(& dhu_listener.as_mut().unwrap()).await {
                hu_tcp = Some(s);
            } else {
                // notify main loop to restart
                //let _ = need_restart.send(None);
                //drop(dhu_listener);
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        } else {
            info!(
                "{} 📂 Opening USB accessory device: <u>{}</u>",
                NAME, USB_ACCESSORY_PATH
            );
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create(false)
                .open(USB_ACCESSORY_PATH)
                .await
            {
                Ok(s) => hu_usb = Some(s),
                Err(e) => {
                    error!("{} 🔴 Error opening USB accessory: {}", NAME, e);
                    // notify main loop to restart
                    //let _ = need_restart.send(None);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }
        }

        info!("{} ♾️ Starting to proxy data between HU and MD...", NAME);
        let started = Instant::now();

        // `read` and `write` take owned buffers (more on that later), and
        // there's no "per-socket" buffer, so they actually take `&self`.
        // which means we don't need to split them into a read half and a
        // write half like we'd normally do with "regular tokio". Instead,
        // we can send a reference-counted version of it. also, since a
        // tokio-uring runtime is single-threaded, we can use `Rc` instead of
        // `Arc`.
        let stats_w_bytes = Arc::new(AtomicUsize::new(0));
        let stats_r_bytes = Arc::new(AtomicUsize::new(0));
        // mpsc channels:
        let (txr_hu, rxr_hu):       (Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
        let (tx_srv, rx_srv):   (Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
        let (txr_srv, rxr_srv): (Sender<Packet>, Receiver<Packet>) = mpsc::channel(20);

        let mut tsk_ch_manager;
        let mut tsk_hu_read;
        let mut tsk_packet_proxy;
        // these will be used for cleanup
        let mut hu_tcp_stream = None;


        // selecting I/O device for reading and writing
        // and creating desired objects for proxy functions
        let hu_r;
        let hu_w;

        // HU transfer device
        if let Some(hu) = hu_usb {
            // HU connected directly via USB
            let hu = Rc::new(hu);
            hu_r = IoDevice::EndpointIo(hu.clone());
            hu_w = IoDevice::EndpointIo(hu.clone());
        } else {
            // Head Unit Emulator via TCP
            let hu = Rc::new(hu_tcp.unwrap());
            hu_r = IoDevice::TcpStreamIo(hu.clone());
            hu_w = IoDevice::TcpStreamIo(hu.clone());
            hu_tcp_stream = Some(hu.clone());
        }

        //service packet proxy
        tsk_packet_proxy = tokio_uring::spawn(packet_tls_proxy(hu_w, rxr_hu, rxr_srv, tx_srv, rx_scrcpy.clone(), stats_r_bytes.clone(), stats_w_bytes.clone(), hex_requested));

        // dedicated reading threads:
        tsk_hu_read = tokio_uring::spawn(endpoint_reader(hu_r, txr_hu));
        
        // main processing threads:
        tsk_ch_manager = tokio_uring::spawn(ch_proxy(
            rx_srv,
            txr_srv,
            tx_scrcpy_cmd.clone(),
            rx_scrcpy_srv_cmd.clone(),
            tx_scrcpy_control.clone(),
        ));
        
        // Thread for monitoring transfer
        let mut tsk_monitor = tokio::spawn(transfer_monitor(
            stats_interval,
            stats_w_bytes,
            stats_r_bytes,
            read_timeout,
            shared_config.clone(),
        ));

        // Stop as soon as one of them errors
        let res = tokio::try_join!(
            flatten(&mut tsk_hu_read, "tsk_hu_read".into()),
            flatten(&mut tsk_ch_manager, "tsk_ch_manager".into()),
            flatten(&mut tsk_monitor,"tsk_monitor".into()),
            flatten(&mut tsk_packet_proxy,"tsk_pkt_proxy".into()),
            flatten(&mut tsk_adb,"tsk_adb_scrcpy".into())
        );

        if let Err(e) = res {
            error!("{} 🔴 Connection error: {}", NAME, e);
        }

        // Make sure the reference count drops to zero and the socket is
        // freed by aborting both tasks (which both hold a `Rc<TcpStream>`
        // for each direction)
        tsk_packet_proxy.abort();
        tsk_hu_read.abort();
        tsk_ch_manager.abort();
        tsk_monitor.abort();
        tsk_adb.abort();

        // make sure TCP connections are closed before next connection attempts
        if let Some(stream) = hu_tcp_stream {
            info!("{} 🛰️ DHU TCP server: closing client connection...", NAME );
            let _ = stream.shutdown(std::net::Shutdown::Both);

        }

        // set webserver context EV stuff to None
        let mut tx_lock = tx.lock().await;
        *tx_lock = None;


        info!("{} ⌛ session time: {}", NAME, format_duration(started.elapsed()).to_string());
        // obtain action for passing it to broadcast sender
        let action = shared_config.read().await.action_requested.clone();
        // stream(s) closed, notify main loop to restart
        let _ = need_restart.send(action);
    }
}
