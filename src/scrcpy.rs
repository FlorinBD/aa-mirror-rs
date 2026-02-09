use std::cmp::min;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use flume::SendError;
use serde::{Deserialize, Serialize};
use simplelog::{error, info};
use tokio::process::Command;
use tokio::sync::mpsc::Receiver;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_uring::net::TcpStream;
use tokio_util::bytes::BytesMut;
use crate::aa_services::{AudioStreamingParams, VideoStreamingParams};
use crate::adb;
use crate::channel_manager::{Packet, ENCRYPTED, FRAME_TYPE_FIRST, FRAME_TYPE_LAST};
use crate::config::{AppConfig, SCRCPY_AUDIO_CODEC, SCRCPY_METADATA_HEADER_LEN, SCRCPY_PORT, SCRCPY_VERSION};
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use protos::*;
use protos::ControlMessageType::{self, *};
use protobuf::{Message};
use tokio::io::AsyncReadExt;

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
async fn tsk_scrcpy_video(
    mut stream: TcpStream,
    ack_notify:Arc<Notify>,
    video_tx: flume::Sender<Packet>,
    max_unack:u32,
    sid:u8,
) -> Result<()> {
    info!("Starting video server!");
    let mut dbg_counter=0;

    //codec metadata
    let metadata=read_exact(&mut stream, 12).await?;
    info!("SCRCPY Video codec metadata: {:02x?}", &metadata);
    let mut codec_id=String::from_utf8_lossy(&metadata[0..4]).to_string();
    codec_id=codec_id.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    let video_res_w = u32::from_be_bytes(metadata[4..8].try_into().unwrap());
    let video_res_h = u32::from_be_bytes(metadata[8..12].try_into().unwrap());
    info!("SCRCPY Video metadata decoded: id={}, res_w={}, res_h={}", codec_id, video_res_w, video_res_h);
    if (codec_id != "h264".to_string()) || (video_res_w != 800) || (video_res_h != 480) {
        error!("SCRCPY Invalid Video codec configuration");
        return Err(Box::new(io::Error::new(io::ErrorKind::Other, "SCRCPY Invalid Video codec configuration")));
    }
    info!("SCRCPY Video entering main loop");
    //let mut reassembler = NalReassembler::new();
    let mut act_unack =0;
    let mut dbg_count=0;
    loop {
        //Read video frames from SCRCPY server
        match read_scrcpy_packet(&mut stream).await {
            Ok((pts, h264_data)) => {
                let mut payload: Vec<u8>=Vec::new();
                let key_frame = (pts & 0x4000_0000_0000_0000u64) != 0;
                let rec_ts = pts & 0x3FFF_FFFF_FFFF_FFFFu64;
                let config_frame = (pts & 0x8000_0000_0000_0000u64) != 0;
                let rd_len = h264_data.len();
                let dbg_len = min(rd_len, 16);
                if dbg_count <  10
                {
                    if rd_len > dbg_len
                    {
                        let end_offset = rd_len - dbg_len;
                        info!("Video task got frame config={:?}, act size: {}, raw slice: {:02x?}...{:02x?}",config_frame, rd_len, &h264_data[..dbg_len], &h264_data[end_offset..]);
                    } else {
                        info!("Video task got frame config={:?}, act size: {}, raw bytes: {:02x?}",config_frame, rd_len, &h264_data[..dbg_len]);
                    }
                    dbg_count += 1;
                }
                if config_frame
                {
                    payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16).to_be_bytes());
                    payload.extend_from_slice(&h264_data);
                }
                else
                {
                    payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_DATA as u16).to_be_bytes());
                    payload.extend_from_slice(&rec_ts.to_be_bytes());
                    payload.extend_from_slice(&h264_data);
                }

                let mut pkt_rsp = Packet {
                    channel: sid,
                    flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload,
                };
                match pkt_rsp.split().await
                {
                    Ok(chunks)=>
                        {
                            for chunk in chunks
                            {
                                match video_tx.send_async(chunk).await
                                {
                                    Ok(_) => {

                                    }
                                    Err(e) => {
                                        error!("Error sending video chunk: {:?}",e);
                                        return Err(Box::new(io::Error::new(io::ErrorKind::Other, "Error sending video chunk")));
                                    }
                                }
                            }
                            act_unack+=1;
                        }
                    _ =>
                        {
                            error!("Video task failed to split packet");
                        }
                }

            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                error!("scrcpy video stream ended");
                return Err(Box::from(e));
            }
            Err(e) => {
                error!("scrcpy video read failed: {}", e);
                return Err(Box::from(e));
            }
        }
        if act_unack >= max_unack
        {
            info!("Video ACK limit hit, waiting new ACK");
            ack_notify.notified().await;
            act_unack=0;
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
            let mut chunk = vec![0u8; to_read];

            let (res, nchunk) = stream.read(chunk).await;
            chunk=nchunk;
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

        match read_exact(stream, SCRCPY_METADATA_HEADER_LEN).await {
            // First 12 bytes = packet metadata
            Ok(metadata) => {
                //info!("SCRCPY video packet metadata: {:02x?}",&metadata);
                if metadata.len() != SCRCPY_METADATA_HEADER_LEN {
                    error!("read_scrcpy_packet data len error, wanted {} but got {} bytes", SCRCPY_METADATA_HEADER_LEN, metadata.len());
                }
                let pts = u64::from_be_bytes(metadata[0..8].try_into().unwrap());
                let packet_size = u32::from_be_bytes(metadata[8..].try_into().unwrap()) as usize;
                match read_exact(stream, packet_size).await {
                    Ok(h264_data) => {
                        // Read full packet
                        if h264_data.len() != packet_size {
                            error!("read_scrcpy_packet data len error, wanted {} but got {} bytes", packet_size, h264_data.len());
                        }
                        Ok((pts, h264_data))
                    }
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        error!("scrcpy stream ended");
                        return Err(e);
                    }
                    Err(e) => {
                        error!("scrcpy read failed: {}", e);
                        return Err(e);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                error!("scrcpy stream ended");
                return Err(e);
            }
            Err(e) => {
                error!("scrcpy read failed: {}", e);
                return Err(e);
            }
        }

    }
}

async fn tsk_scrcpy_audio(
    mut stream: TcpStream,
    mut ack_notify:Arc<Notify>,
    audio_tx: flume::Sender<Packet>,
    max_unack:u32,
    sid:u8
) -> Result<()> {
    let mut dbg_counter=0;
    info!("Starting audio server!");
    //codec metadata
    let metadata=read_exact(&mut stream, 4).await?;
    info!("SCRCPY Audio codec metadata: {:02x?}", &metadata);
    let mut codec_id=String::from_utf8_lossy(&metadata[0..4]).to_string();
    codec_id=codec_id.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    info!("SCRCPY Audio codec id: {}", codec_id);
    if codec_id != SCRCPY_AUDIO_CODEC.to_string() {
        error!("SCRCPY Invalid audio codec configuration");
        return Err(Box::new(io::Error::new(io::ErrorKind::Other, "SCRCPY Invalid audio codec configuration")));
    }
    let mut act_unack =0;
    let mut dbg_count=0;
    loop {
        //Read video frames from SCRCPY server
        match read_scrcpy_packet(&mut stream).await {
            Ok((pts, data)) => {
                let mut payload: Vec<u8>=Vec::new();
                let rd_len = data.len();
                let dbg_len = min(rd_len, 16);
                let rec_ts = pts & 0x3FFF_FFFF_FFFF_FFFFu64;
                let config_frame = (pts & 0x8000_0000_0000_0000u64) != 0;
                if dbg_count <  10
                {
                    if rd_len > dbg_len
                    {
                        let end_offset = rd_len - dbg_len;
                        info!("Audio task got packet, ts={}, act size: {}, raw slice: {:02x?}...{:02x?}",rec_ts, rd_len, &data[..dbg_len], &data[end_offset..]);
                    } else {
                        info!("Audio task got packet, ts={}, act size: {}, raw bytes: {:02x?}",rec_ts, rd_len, &data[..dbg_len]);
                    }
                    dbg_count+=1;
                }
                if config_frame
                {
                    payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16).to_be_bytes());
                    payload.extend_from_slice(&data);
                }
                else
                {
                    payload.extend_from_slice(&(MediaMessageId::MEDIA_MESSAGE_DATA as u16).to_be_bytes());
                    payload.extend_from_slice(&rec_ts.to_be_bytes());
                    payload.extend_from_slice(&data);
                }

                let mut pkt_rsp = Packet {
                    channel: sid,
                    flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload,
                };
                match pkt_rsp.split().await
                {
                    Ok(chunks)=>
                        {
                            for chunk in chunks
                            {
                                match audio_tx.send_async(chunk).await
                                {
                                    Ok(_) => {
                                    }
                                    Err(e) => {
                                        error!("Error sending audio chunk: {:?}", e);
                                        return Err(Box::new(io::Error::new(io::ErrorKind::Other, "Error sending audio chunk")));
                                    }
                                }
                            }
                            act_unack+=1;
                        }
                    _ =>
                        {
                            error!("Audio task failed to split packet");
                        }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                error!("scrcpy audio stream ended");
                return Err(Box::from(e));
            }
            Err(e) => {
                error!("scrcpy audio read failed: {}", e);
                return Err(Box::from(e));
            }
        }
        if (act_unack >= max_unack) && (max_unack>0)
        {
            info!("Audio ACK limit hit, waiting new ACK");
            ack_notify.notified().await;
            act_unack=0;
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
            let mut chunk = vec![0u8; to_read];

            let (res, nchunk) = stream.read(chunk).await;
            chunk=nchunk;
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

        match read_exact(stream, SCRCPY_METADATA_HEADER_LEN).await {
            // First 12 bytes = packet metadata
            Ok(metadata) => {
                //info!("SCRCPY video packet metadata: {:02x?}",&metadata);
                if metadata.len() != SCRCPY_METADATA_HEADER_LEN {
                    error!("read_scrcpy_packet data len error, wanted {} but got {} bytes", SCRCPY_METADATA_HEADER_LEN, metadata.len());
                }
                let pts = u64::from_be_bytes(metadata[0..8].try_into().unwrap());
                let packet_size = u32::from_be_bytes(metadata[8..].try_into().unwrap()) as usize;
                match read_exact(stream, packet_size).await {
                    Ok(data) => {
                        // Read full packet
                        if data.len() != packet_size {
                            error!("read_scrcpy_packet data len error, wanted {} but got {} bytes", packet_size, data.len());
                        }
                        Ok((pts, data))
                    }
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        error!("scrcpy audio stream ended");
                        return Err(e);
                    }
                    Err(e) => {
                        error!("scrcpy audio read failed: {}", e);
                        return Err(e);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                error!("scrcpy audio stream ended");
                return Err(e);
            }
            Err(e) => {
                error!("scrcpy audio read failed: {}", e);
                return Err(e);
            }
        }

    }
}

async fn tsk_scrcpy_control(
    stream: TcpStream,
    cmd_rx: flume::Receiver<Packet>,
    screen_size:ScrcpySize,
    cfg_screen_off:bool,
) -> Result<()> {

    info!("Starting control server!");
    if cfg_screen_off {
        let mut payload: Vec<u8> = Vec::new();
        payload.push(ScrcpyControlMessageType::SetDisplayPower as u8);
        payload.push(0);
        //stream.write_all(payload).await;
        let (res, _) = stream.write_all(payload).await;
        if let Err(e) = res {
            error!("tsk_scrcpy_control send error: {}", e);
        }
    }
    loop {
        match cmd_rx.recv_async().await {
            Ok(pkt) => {
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
                                //let sz = ScrcpySize { width: video_params.res_w as u16, height: video_params.res_h as u16 };
                                let pos = ScrcpyPosition { point: pt, screen_size: screen_size.clone() };
                                let ev = ScrcpyTouchEvent { action: _action, pointer_id: pointer_id as u64, position: pos, pressure: 0xffff, action_button: 1, buttons: 1 };//AMOTION_EVENT_BUTTON_PRIMARY
                                //info!("SCRCPY Control inject event: {:?}",ev);
                                let ev_bytes=ev.to_be_bytes();
                                let mut payload: Vec<u8> = Vec::new();
                                payload.push(ScrcpyControlMessageType::InjectTouchEvent as u8);
                                payload.extend_from_slice(&ev_bytes);
                                //stream.write_all(payload).await;
                                let (res, _) = stream.write_all(payload).await;
                                if let Err(e) = res {
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
                                let pos = ScrcpyPosition { point: pt, screen_size: screen_size.clone() };
                                let ev = ScrcpyTouchEvent { action: _action, pointer_id: pointer_id as u64, position: pos, pressure: 0xffff, action_button: 1, buttons: 1 };//AMOTION_EVENT_BUTTON_PRIMARY
                                //info!("SCRCPY Control inject event: {:?}",ev);
                                let ev_bytes=ev.to_be_bytes();
                                let mut payload: Vec<u8> = Vec::new();
                                payload.push(ScrcpyControlMessageType::InjectTouchEvent as u8);
                                payload.extend_from_slice(&ev_bytes);
                                //stream.write_all(payload).await;
                                let (res, _) = stream.write_all(payload).await;
                                if let Err(e) = res {
                                    error!("tsk_scrcpy_control send error: {}", e);
                                }
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
                                //info!("SCRCPY Control inject event: {:?}",ev);
                                let ev_bytes=ev.to_be_bytes();
                                let mut payload: Vec<u8> = Vec::new();
                                payload.push(ScrcpyControlMessageType::InjectKeycode as u8);
                                payload.extend_from_slice(&ev_bytes);
                                //stream.write_all(payload).await;
                                let (res, _) = stream.write_all(payload).await;
                                if let Err(e) = res {
                                    error!("tsk_scrcpy_control send error: {}", e);
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
                    let cmd_id: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
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
            Err(flume::RecvError::Disconnected) => {
                // Sender has been dropped, exit loop
                println!("Sender closed, exiting scrcpy control loop");
                break;
            }
        }
    }
    Err(Box::new(flume::RecvError::Disconnected))
}
///This task is not meant to be closed, it will always run
pub(crate) async fn tsk_adb_scrcpy(
    media_tx: flume::Sender<Packet>,
    srv_cmd_rx_scrcpy: flume::Receiver<Packet>,
    srv_cmd_tx: flume::Sender<Packet>,
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
            let mut start_audio_recived=false;
            let mut start_video_recived=false;
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
                                        start_video_recived=true;
                                        if start_audio_recived && start_video_recived
                                        {
                                            break;
                                        }

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
                                        start_audio_recived=true;
                                        if start_audio_recived && start_video_recived
                                        {
                                            break;
                                        }
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
            cmd_shell.push("display_ime_policy=local".to_string());
            cmd_shell.push("stay_awake=true".to_string());
            cmd_shell.push(format!("audio_codec={}",SCRCPY_AUDIO_CODEC.to_string() ));
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
                tokio::time::sleep(Duration::from_secs(1)).await;//give some time to start sockets
                let addr = format!("127.0.0.1:{}", SCRCPY_PORT).parse().unwrap();
                let video_stream = TcpStream::connect(addr).await?;
                video_stream.set_nodelay(true)?;
                let addr = format!("127.0.0.1:{}", SCRCPY_PORT).parse().unwrap();
                let audio_stream = TcpStream::connect(addr).await?;
                audio_stream.set_nodelay(true)?;
                let addr = format!("127.0.0.1:{}", SCRCPY_PORT).parse().unwrap();
                let ctrl_stream = TcpStream::connect(addr).await?;
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

                //mpsc channels for ACK notification, to maintain frame window
                let mut audio_max_unack_mpsc = 1usize;
                let mut video_max_unack_mpsc =1usize;
                if audio_codec_params.max_unack > 0
                {
                    audio_max_unack_mpsc =audio_codec_params.max_unack as usize;
                }
                if video_codec_params.max_unack > 0
                {
                    video_max_unack_mpsc =video_codec_params.max_unack as usize;
                }
                //let (tx_ack_audio, rx_ack_audio) = flume::bounded::<u32>(1);
                //let (tx_ack_video, rx_ack_video) = flume::bounded::<u32>(1);

                let notify_audio = Arc::new(Notify::new());
                let notify_video = Arc::new(Notify::new());
                let ack_audio=notify_audio.clone();
                let ack_video=notify_video.clone();
                let (tx_ctrl, rx_ctrl)=flume::bounded::<Packet>(5);
                hnd_scrcpy_video = tokio_uring::spawn(async move {
                    let res = tsk_scrcpy_video(
                        video_stream,
                        notify_video.clone(),
                        video_tx,
                        video_codec_params.max_unack,
                        video_codec_params.sid
                    ).await;
                    let _ = done_th_tx_video.send(res);

                });
                hnd_scrcpy_audio = tokio_uring::spawn(async move {
                    let res = tsk_scrcpy_audio(
                        audio_stream,
                        notify_audio.clone(),
                        audio_tx,
                        audio_codec_params.max_unack,
                        audio_codec_params.sid,
                    ).await;
                    let _ = done_th_tx_audio.send(res);
                });

                let screen_size=ScrcpySize{ width: video_codec_params.res_w as u16, height: video_codec_params.res_h as u16 };
                hnd_scrcpy_ctrl = tokio_uring::spawn(async move {
                    let res = tsk_scrcpy_control(
                        ctrl_stream,
                        rx_ctrl,
                        screen_size,
                        config.scrcpy_screen_off.clone(),
                    ).await;
                    let _ = done_th_tx_ctrl.send(res);
                });


                info!("Connected to control server!");
                let mut buf = [0u8; 1024];
                loop {
                    tokio::select! {
                    biased;
                        Ok(pkt)=srv_cmd_rx_scrcpy.recv_async() => {
                            //info!("tsk_scrcpy_video Received command packet {:02x?}", pkt);
                            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
                            if message_id == MESSAGE_CUSTOM_CMD as i32
                            {
                                let cmd_id: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                                if cmd_id == CustomCommand::CMD_STOP_VIDEO_RECORDING as i32
                                {

                                    info!("tsk_scrcpy_video Video streaming stopped");
                                    //drop(tx_ack_video);
                                    hnd_scrcpy_video.abort();
                                    //FIXME close the stream
                                    break;
                                }
                                else if cmd_id == CustomCommand::CMD_STOP_AUDIO_RECORDING as i32
                                {

                                    info!("tsk_scrcpy_video Audio streaming stopped");
                                    //drop(tx_ack_audio);
                                    hnd_scrcpy_audio.abort();
                                    //FIXME close the stream
                                    break;
                                }
                                else if cmd_id == CustomCommand::CANCEL as i32
                                {

                                    info!("tsk_scrcpy CANCEL CMD received, stopping all tasks");
                                    hnd_scrcpy_audio.abort();
                                    hnd_scrcpy_video.abort();
                                    //drop(tx_ack_audio);
                                    //drop(tx_ack_video);
                                    if let Err(_) = tx_ctrl.send_async(pkt).await
                                    {
                                        error!( "tsk_scrcpy control proxy send error, buffer full?");
                                    };
                                    break;
                                }
                            }
                            else if message_id == MediaMessageId::MEDIA_MESSAGE_ACK as i32
                            {
                                //info!("{} Received {} message", sid.to_string(), message_id);
                                let data = &pkt.payload[2..]; // start of message data, without message_id
                                if let Ok(_) = Ack::parse_from_bytes(&data)
                                {
                                    if pkt.channel == video_sid
                                    {
                                        info!("tsk_scrcpy: video ACK recived");
                                        ack_video.notify_one();
                                    }
                                    else if pkt.channel == audio_sid
                                    {
                                        info!("tsk_scrcpy: audio ACK recived");
                                        ack_audio.notify_one();
                                    }
                                    else
                                    {
                                        error!("tsk_scrcpy unexpected channel ID for ACK command");
                                    }
                                }
                                else
                                {
                                    error!( "tsk_scrcpy Unable to parse MEDIA_MESSAGE_ACK message");
                                }
                            }
                            else if message_id == InputMessageId::INPUT_MESSAGE_INPUT_REPORT  as i32
                            {
                                if let Err(_) = tx_ctrl.send_async(pkt).await
                                {
                                    error!( "tsk_scrcpy control proxy send error, buffer full?");
                                };
                            }
                            else
                            {
                                error!("tsk_scrcpy_video unknown message id: {}", message_id);
                            }
                        }
                        Ok(n) = sh_reader.read(&mut buf) => {
                            if(n>2)
                            {
                                info!("shell stdout: {}", String::from_utf8_lossy(&buf[..n]));
                            }
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
                //FIXME cancel all tasks
                //Stop CONTROL task as well
                let mut payload: Vec<u8>=Vec::new();
                payload.extend_from_slice(&(MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                payload.extend_from_slice(&(CustomCommand::CANCEL as u16).to_be_bytes());
                let pkt_rsp = Packet {
                    channel: 0,
                    flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: payload,
                };
                if let Err(_) = tx_ctrl.send_async(pkt_rsp).await{
                    error!( "scrcpy CANCEL send error");
                };
                // When done, stop the shell
                //drop(rx_scrcpy_ctrl);
                shell.kill().await?;
                info!("Sending MD_DISCONNECTED to inform services");
                let mut payload: Vec<u8>=Vec::new();
                payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                payload.extend_from_slice(&(CustomCommand::MD_DISCONNECTED as u16).to_be_bytes());
                let pkt_rsp = Packet {
                    channel: 0,
                    flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                    final_length: None,
                    payload: std::mem::take(&mut payload),
                };
                if let Err(_) = srv_cmd_tx.send_async(pkt_rsp).await{
                    error!( "scrcpy MD_DISCONNECTED send error");
                };
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