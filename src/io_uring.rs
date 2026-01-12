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
use crate::aa_services::{CmdStartVideoRec};
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use protos::*;
use protos::ControlMessageType::{self, *};
use protobuf::{Message};
use std::cmp::min;

// module name for logging engine
const NAME: &str = "<i><bright-black> io_uring: </>";

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const USB_ACCESSORY_PATH: &str = "/dev/usb_accessory";
pub const BUFFER_LEN: usize = 16 * 1024;
pub const TCP_CLIENT_TIMEOUT: Duration = Duration::new(30, 0);

use crate::config::{Action, AppConfig, SharedConfig, ADB_DEVICE_PORT, SCRCPY_VERSION, SCRCPY_PORT};
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
    mut cmd_rx: broadcast::Receiver<Packet>,
    video_tx: broadcast::Sender<Packet>,
) -> Result<()> {
    info!("Starting video server!");
    let mut streaming_on=false;
    let mut buf = vec![0u8; 0xffff];
    let codec_buf = vec![0u8; 12];
    let mut i=0;
    let mut ch_id:u8=0;

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
    info!("SCRCPY Video codec metadata: {:?}", &buf_out[..n]);
    let mut codec_id=String::from_utf8_lossy(&buf_out[0..4]).to_string();
        codec_id=codec_id.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    //let codec_id = u32::from_be_bytes(buf_out[0..4].try_into().unwrap());
    let video_res_w = u32::from_be_bytes(buf_out[4..4].try_into().unwrap());
    let video_res_h = u32::from_be_bytes(buf_out[8..4].try_into().unwrap());
    info!("SCRCPY Video metadata decoded: id={}, res_w={}, res_h={}", codec_id, video_res_w, video_res_h);
    /*if (codec_id != "h264".to_string()) || (video_res_w != 800) || (video_res_h != 480) {
        error!("SCRCPY Invalid Video codec configuration");
        return Err(Box::new(io::Error::new(
            io::ErrorKind::Other,
            "SCRCPY Invalid Video codec configuration",
        )));
    }*/
    let timestamp: u64 = 0;//is not used by HU
    loop {
        //TODO read packet size, not all available
        let (res, buf_out) = stream.read(buf).await;
        let n = res?;
        if n == 0 {
            error!("Video connection closed by server?");
            return Err(Box::new(io::Error::new(
                io::ErrorKind::Other,
                "Video connection closed by server?",
            )));
        }
        let dbg_len=min(n,16);
        if i<5
        {
            info!("Video task Read {} bytes: {:?}", n, &buf_out[..dbg_len]);
            i=i+1;
        }
        if streaming_on
        {
            let pts = u64::from_be_bytes(buf_out[0..8].try_into().unwrap());
            let mut payload: Vec<u8>=Vec::new();

            if (pts & 0x8000000000000000) >0
            {
                payload.extend_from_slice(&buf_out[12..n-12]);
                payload.insert(0, ((MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16) >> 8) as u8);
                payload.insert(1, ((MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG as u16) & 0xff) as u8);
            }
            else {

                payload.extend_from_slice(&timestamp.to_be_bytes());
                payload.extend_from_slice(&buf_out[12..n-12]);
                payload.insert(0, ((MediaMessageId::MEDIA_MESSAGE_DATA as u16) >> 8) as u8);
                payload.insert(1, ((MediaMessageId::MEDIA_MESSAGE_DATA as u16) & 0xff) as u8);
            }

            let pkt_rsp = Packet {
                channel: ch_id,
                flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                final_length: None,
                payload: payload,
            };
            if let Err(_) = video_tx.send(pkt_rsp){
                error!( "SCRCPY video frame send error");
            };
        }
        // Reuse buffer
        buf = buf_out;
        //Check custom Service command
        match cmd_rx.try_recv() {
            Ok(pkt) => {
                info!("tsk_scrcpy_video Received command packet {:?}", pkt);
                let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
                if message_id == MESSAGE_CUSTOM_CMD  as i32
                {
                    let data = &pkt.payload[2..]; // start of message data, without message_id
                    if let Ok(msg) = CustomCommandMessage::parse_from_bytes(&data) {
                        if msg.cmd() == CustomCommand::CMD_START_VIDEO_RECORDING
                        {
                            streaming_on=true;
                            if let Ok(cmd) = postcard::from_bytes::<CmdStartVideoRec>(&data[4..]) {
                                ch_id=pkt.channel;
                            }
                            else {
                                error!("tsk_scrcpy_video parsing error for CmdStartVideoRec");
                            }
                        }
                        else if msg.cmd() == CustomCommand::CMD_STOP_VIDEO_RECORDING
                        {
                            streaming_on=false;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn tsk_scrcpy_audio(
    mut stream: TcpStream,
    mut cmd_rx: broadcast::Receiver<Packet>,
    audio_tx: broadcast::Sender<Packet>,
) -> Result<()> {

    info!("Starting audio server!");
    let mut streaming_on=false;
    let mut ch_id:u8=0;
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
    info!("SCRCPY Audio codec metadata: {:?}", &buf_out[..n]);
    let mut codec_id=String::from_utf8_lossy(&buf_out[0..4]).to_string();
        codec_id=codec_id.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    info!("SCRCPY Audio codec id: {}", codec_id);
    let mut buf = vec![0u8; 0xffff];
    let mut i=0;
    let timestamp: u64 = 0;//is not used by HU
    loop {
        let (res, buf_out) = stream.read(buf).await;
        let n = res?;
        if n == 0 {
            info!("Audio connection closed by server?");
            return Err(Box::new(io::Error::new(
                io::ErrorKind::Other,
                "Audio connection closed by server",
            )));
        }
        let dbg_len=min(n,16);
        if i<5
        {
            info!("Audio task Read {} bytes: {:?}", n, &buf_out[..dbg_len]);
            i=i+1;
        }
        if streaming_on
        {
            let mut payload: Vec<u8>=Vec::new();
            payload.extend_from_slice(&timestamp.to_be_bytes());
            payload.extend_from_slice(&buf_out[12..n-12]);
            payload.insert(0, ((MediaMessageId::MEDIA_MESSAGE_DATA as u16) >> 8) as u8);
            payload.insert(1, ((MediaMessageId::MEDIA_MESSAGE_DATA as u16) & 0xff) as u8);

            let pkt_rsp = Packet {
                channel: ch_id,
                flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                final_length: None,
                payload: payload,
            };
            if let Err(_) = audio_tx.send(pkt_rsp){
                error!( "SCRCPY audio frame send error");
            };
        }
        // Reuse buffer
        buf = buf_out;
        //Check custom Service command
        match cmd_rx.try_recv() {
            Ok(pkt) => {
                info!("tsk_scrcpy_audio Received command packet {:?}", pkt);
            }
            _ => {}
        }
    }
    Ok(())
}
async fn tsk_adb_scrcpy(
    video_cmd_rx: broadcast::Receiver<Packet>,
    audio_cmd_rx: broadcast::Receiver<Packet>,
    srv_tx: broadcast::Sender<Packet>,
    //audio_tx: Sender<Packet>,
    config: AppConfig,
) -> Result<()> {
    info!("{}: ADB task started",NAME);
    let cmd_adb = Command::new("adb")
        .arg("start-server")
        .output().await.unwrap();
    if !cmd_adb.status.success() {
        error!("ADB server can't start");
    }
    loop
    {
        if let Some(device)=adb::get_first_adb_device(config.clone()).await {
            info!("{}: ADB device found: {:?}, trying to get video/audio from it now",NAME, device);

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
                continue;
            }

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
            tokio::time::sleep(Duration::from_secs(1)).await;//give some time to forward port, it is needed?
            let video_bitrate=8000000;
            let video_res_w=800;
            let video_res_h=480;
            let video_fps=60;
            let screen_dpi=160;
            let audio_bitrate:i32=48000;

            tokio::time::sleep(Duration::from_secs(1)).await;//give some time to map adb sockets
            let mut cmd_shell:Vec<String> = vec![];
            //cmd_shell.push(format!("CLASSPATH=/data/local/tmp/scrcpy-server-manual.jar app_process / com.genymobile.scrcpy.Server {} log_level=info send_frame_meta=true tunnel_forward=true audio=true video=true control=true send_dummy_byte=false cleanup=true raw_stream=true audio_codec=aac audio_bit_rate={} max_size={} video_bit_rate={} video_codec=h264 new_display={}x{}/{} max_fps={}",SCRCPY_VERSION.to_string(),SCID_VIDEO.to_string(),audio_bitrate, video_res_w, video_bitrate, video_res_w, video_res_h, screen_dpi, video_fps));
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
            cmd_shell.push(format!("audio_bit_rate={}", audio_bitrate));
            cmd_shell.push(format!("max_size={}", video_res_w));
            cmd_shell.push("video_codec=h264".to_string());
            cmd_shell.push(format!("video_bit_rate={}", video_bitrate));
            cmd_shell.push(format!("new_display={}x{}/{}", video_res_w, video_res_h, screen_dpi));
            cmd_shell.push(format!("max_fps={}", video_fps));
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
                let video_tx = srv_tx.clone();
                let audio_tx = srv_tx.clone();
                let (done_th_tx_video, mut done_th_rx_video) = oneshot::channel();
                let (done_th_tx_audio, mut done_th_rx_audio) = oneshot::channel();
                hnd_scrcpy_video = tokio_uring::spawn(async move {
                    let res = tsk_scrcpy_video(
                                                    video_stream,
                                                    video_cmd_rx.resubscribe(),
                                                    video_tx,
                                                    ).await;
                    let _ = done_th_tx_video.send(res);

                });
                hnd_scrcpy_audio = tokio_uring::spawn(async move {
                    let res = tsk_scrcpy_audio(
                                                    audio_stream,
                                                    audio_cmd_rx.resubscribe(),
                                                    audio_tx,
                                                    ).await;
                    let _ = done_th_tx_audio.send(res);
                });


                info!("Connected to control server!");

                loop {

                    if done_th_rx_video.try_recv().is_ok() {
                        info!("SCRCPY Video Task finished");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    else if done_th_rx_audio.try_recv().is_ok() {
                        info!("SCRCPY Audio Task finished");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    else {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    let mut buf = [0u8; 1024];

                    match timeout(Duration::from_millis(1000), sh_reader.read(&mut buf)).await {
                        Ok(Ok(n)) if n > 0 => {
                            println!("shell stdout: {}", String::from_utf8_lossy(&buf[..n]));
                        }
                        Ok(Ok(_)) => {
                            // EOF
                        }
                        Ok(Err(e)) => {
                            eprintln!("read error: {}", e);
                        }
                        Err(_) => {
                            // no data yet (non-blocking behavior)
                        }
                    }
                    //TODO check audio/video task if they finished to restart connection
                }
                // When done, stop the shell
                shell.kill().await?;
            }
            else {
                error!("Invalid response for ADB shell");
                continue;
            }
            //FIXME add a cancellation token
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

    //mpsc for scrcpy
    let (tx_cmd_audio, rx_cmd_audio)=broadcast::channel::<Packet>(5);
    let (tx_cmd_video, rx_cmd_video)=broadcast::channel::<Packet>(5);
    let (tx_scrcpy, rx_scrcpy)=broadcast::channel::<Packet>(30);

    let mut tsk_adb;
    tsk_adb = tokio_uring::spawn(tsk_adb_scrcpy(
        rx_cmd_video,
        rx_cmd_audio,
        tx_scrcpy,
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
        tsk_packet_proxy = tokio_uring::spawn(packet_tls_proxy(hu_w, rxr_hu, rxr_srv, tx_srv, rx_scrcpy.resubscribe(), stats_r_bytes.clone(), stats_w_bytes.clone(), hex_requested));

        // dedicated reading threads:
        tsk_hu_read = tokio_uring::spawn(endpoint_reader(hu_r, txr_hu));
        
        // main processing threads:
        tsk_ch_manager = tokio_uring::spawn(ch_proxy(
            rx_srv,
            txr_srv,
            tx_cmd_video.clone(),
            tx_cmd_audio.clone()
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
