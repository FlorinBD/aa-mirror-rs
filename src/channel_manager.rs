use anyhow::{anyhow, Context};
use log::log_enabled;
use openssl::ssl::{ErrorCode, Ssl, SslContextBuilder, SslFiletype, SslMethod};
use simplelog::*;
use std::collections::{HashMap, VecDeque};
use std::{fmt, io};
use std::cmp::PartialEq;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::timeout;
use tokio_uring::buf::BoundedBuf;

// protobuf stuff:
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use crate::channel_manager::protos::auth_response::Status::*;
use crate::channel_manager::protos::*;
use crate::channel_manager::protos::Config as ChConfig;
use crate::channel_manager::AudioStreamType::*;
use crate::channel_manager::MessageStatus;
use protobuf::text_format::print_to_string_pretty;
use protobuf::{Enum, Message, MessageDyn};
use tokio::sync::{mpsc};
use tokio::task::JoinHandle;
use tokio_uring::net::TcpStream;
use tokio_util::sync::CancellationToken;
//use protos::ControlMessageType::{self, *};
use protos::{ControlMessageType, MediaMessageId};
use crate::aa_services::{VideoCodecResolution::*, VideoFPS::*, AudioStream, AudioConfig, MediaCodec::*, ServiceType, CommandState, ServiceStatus, VideoStreamingParams, AudioStreamingParams, SensorType, AAMessageType, SrvSensorSource, AAService};
use crate::config::{AppConfig, SharedConfig, DHU_MAKE_DEV, DHU_MODEL_DEV, HU_CONFIG_DELAY_MS, MAX_DATA_LEN, MAX_PACKET_LEN};
use crate::config_types::{AAMode, HexdumpLevel};
use crate::io_uring::Endpoint;
use crate::io_uring::{IoDevice, Result};
use crate::io_uring::BUFFER_LEN;

// module name for logging engine
fn get_name() -> String {
    let dev = "CH Manager";
    format!("<i><bright-black>{}: </>", dev)
}

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
//type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;


// message related constants:
pub const HEADER_LENGTH: usize = 4;
pub const FRAME_TYPE_FIRST: u8 = 1 << 0;
pub const FRAME_TYPE_LAST: u8 = 1 << 1;
pub const FRAME_TYPE_MASK: u8 = FRAME_TYPE_FIRST | FRAME_TYPE_LAST;
pub const FRAME_TYPE_CONTROL: u8 = 1 << 2;
pub const ENCRYPTED: u8 = 1 << 3;

// location for hu_/md_ private keys and certificates:
pub const KEYS_PATH: &str = "/etc/aa-mirror-rs";
pub(crate) const RES_PATH: &str = "/etc/aa-mirror-rs/res";

#[derive(PartialEq, Copy, Clone, Debug)]
pub enum DeviceType {
    HeadUnit,
    MobileDevice,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct CmdStatus {
    pub status: CommandState,
}
/// rust-openssl doesn't support BIO_s_mem
/// This SslMemBuf is about to provide `Read` and `Write` implementations
/// to be used with `openssl::ssl::SslStream`
/// more info:
/// https://github.com/sfackler/rust-openssl/issues/1697
type LocalDataBuffer = Arc<Mutex<VecDeque<u8>>>;
#[derive(Clone)]
pub struct SslMemBuf {
    /// a data buffer that the server writes to and the client reads from
    pub server_stream: LocalDataBuffer,
    /// a data buffer that the client writes to and the server reads from
    pub client_stream: LocalDataBuffer,
}

// Read implementation used internally by OpenSSL
impl Read for SslMemBuf {
    fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, std::io::Error> {
        self.client_stream.lock().unwrap().read(buf)
    }
}

// Write implementation used internally by OpenSSL
impl Write for SslMemBuf {
    fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, std::io::Error> {
        self.server_stream.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::result::Result<(), std::io::Error> {
        self.server_stream.lock().unwrap().flush()
    }
}

// Own functions for accessing shared data
impl SslMemBuf {
    fn read_to(&mut self, buf: &mut Vec<u8>) -> std::result::Result<usize, std::io::Error> {
        self.server_stream.lock().unwrap().read_to_end(buf)
    }
    fn write_from(&mut self, buf: &[u8]) -> std::result::Result<usize, std::io::Error> {
        self.client_stream.lock().unwrap().write(buf)
    }
}
#[derive(Debug, Clone)]
pub struct Packet {
    pub channel: u8,
    pub flags: u8,
    pub final_length: Option<u32>,
    pub payload: Vec<u8>,
}

impl Packet {
    /// payload encryption if needed
    async fn encrypt_payload(
        &mut self,
        mem_buf: &mut SslMemBuf,
        server: &mut openssl::ssl::SslStream<SslMemBuf>,
    ) -> Result<()> {
        if (self.flags & ENCRYPTED) == ENCRYPTED {
            // save plain data for encryption
            server.ssl_write(&self.payload)?;
            // read encrypted data
            let mut res: Vec<u8> = Vec::new();
            mem_buf.read_to(&mut res)?;
            self.payload = res;
        }

        Ok(())
    }

    /// payload decryption if needed
    async fn decrypt_payload(
        &mut self,
        mem_buf: &mut SslMemBuf,
        server: &mut openssl::ssl::SslStream<SslMemBuf>,
    ) -> Result<()> {
        if (self.flags & ENCRYPTED) == ENCRYPTED {
            // save encrypted data
            mem_buf.write_from(&self.payload)?;
            // read plain data
            let mut res: Vec<u8> = Vec::new();
            server.read_to_end(&mut res)?;
            self.payload = res;
        }

        Ok(())
    }


    /// composes a final frame and transmits it to endpoint device (HU/MD)
    pub async fn transmit<A: Endpoint<A>>(
        &self,
        device: &mut IoDevice<A>,
    ) -> std::result::Result<usize, std::io::Error> {
        let len = self.payload.len() as u16;
        let mut frame: Vec<u8> = vec![];
        frame.push(self.channel);
        frame.push(self.flags);
        frame.push((len >> 8) as u8);
        frame.push((len & 0xff) as u8);
        if let Some(final_len) = self.final_length {
            // adding addional 4-bytes of final_len header
            frame.push((final_len >> 24) as u8);
            frame.push((final_len >> 16) as u8);
            frame.push((final_len >> 8) as u8);
            frame.push((final_len & 0xff) as u8);
        }
        match device {
            IoDevice::UsbWriter(device, _) => {
                frame.append(&mut self.payload.clone());
                let mut dev = device.borrow_mut();
                dev.write(&frame).await
            }
            IoDevice::EndpointIo(device) => {
                frame.append(&mut self.payload.clone());
                device.write(frame).submit().await.0
            }
            IoDevice::TcpStreamIo(device) => {
                frame.append(&mut self.payload.clone());
                device.write(frame).submit().await.0
            }
            _ => todo!(),
        }
    }

    /// decapsulates SSL payload and writes to SslStream
    async fn ssl_decapsulate_write(&self, mem_buf: &mut SslMemBuf) -> Result<()> {
        let message_type = u16::from_be_bytes(self.payload[0..=1].try_into()?);
        if message_type == ControlMessageType::MESSAGE_ENCAPSULATED_SSL as u16 {
            mem_buf.write_from(&self.payload[2..])?;
        }
        Ok(())
    }
}

impl fmt::Display for Packet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "packet dump:\n")?;
        write!(f, " channel: {:02X}\n", self.channel)?;
        write!(f, " flags: {:02X}\n", self.flags)?;
        write!(f, " final length: {:04X?}\n", self.final_length)?;
        write!(f, " payload: {:02X?}\n", self.payload.clone().into_iter())?;

        Ok(())
    }
}

pub struct AckChannels {
    pub(crate) audio_rx: Receiver<()>,
    pub(crate) video_rx: Receiver<()>,
    pub(crate) audio_sid:u8,
    pub(crate) video_sid:u8,
}
pub struct ChannelProxyHandle {
    pub(crate) ch_rx: Option<AckChannels>,
    pub(crate) data: Option<Packet>,
}

///Used for AA/Mirror mode as a SSL gateway
pub struct TlsPacketProxy {
    //params
    r_statistics: Arc<AtomicUsize>,
    w_statistics: Arc<AtomicUsize>,
    dmp_level:HexdumpLevel,
    cfg:AppConfig,
    //local vars
    audio_sid:u8,
    video_sid:u8,
    audio_ack_rx: Option<Receiver<()>>,
    video_ack_rx: Option<Receiver<()>>,
}

impl TlsPacketProxy
{
    pub fn new(
        r_statistics: Arc<AtomicUsize>,
        w_statistics: Arc<AtomicUsize>,
        dmp_level: HexdumpLevel,
        cfg: AppConfig,
    ) -> Self {
        Self {
            r_statistics,
            w_statistics,
            dmp_level,
            cfg,
            audio_sid:0,
            video_sid:0,
            audio_ack_rx:None,
            video_ack_rx:None,
        }
    }

    async fn run_aa_mitm<A: Endpoint<A>>(mut self, mut hu_wr: IoDevice<A>,
                                         mut hu_rx: Receiver<Packet>,
                                         mut md_rx: Receiver<Packet>,
                                         mut md_tx: IoDevice<TcpStream>,
    ) -> Result<()> {
        let ssl_hu = self.ssl_builder_md().await?;
        let ssl_md = self.ssl_builder_hu().await?;
        let mut mem_buf_hu = SslMemBuf {
            client_stream: Arc::new(Mutex::new(VecDeque::new())),
            server_stream: Arc::new(Mutex::new(VecDeque::new())),
        };
        let mut mem_buf_md = SslMemBuf {
            client_stream: Arc::new(Mutex::new(VecDeque::new())),
            server_stream: Arc::new(Mutex::new(VecDeque::new())),
        };
        let mut ssl_handshake_done=false;
        let mut server = openssl::ssl::SslStream::new(ssl_hu, mem_buf_hu.clone())?;
        let mut client = openssl::ssl::SslStream::new(ssl_md, mem_buf_md.clone())?;
        let mut sdr =None;
        let mut sdr_msg_types:HashMap<i32,AAMessageType> = HashMap::new();
        sdr_msg_types.insert(0,AAMessageType::Control);
        let mut ch_id_hu=0;
        info!( "{}: Starting AA MITM message proxy loop...", get_name());
        loop {
            tokio::select! {
            biased;

            // 🔴 highest priority, HU>MD
            Some(mut msg) = hu_rx.recv() => {
                // Increment byte counters for statistics
                // fixme: compute final_len for precise stats
                self.r_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                //debug!("{}: Received {:?} bytes from HU on ch id: {:?}", get_name(), HEADER_LENGTH + msg.payload.len(), msg.channel);
                ch_id_hu=msg.channel as i32;
                if msg.flags&ENCRYPTED !=0
                {
                    if !ssl_handshake_done
                    {
                        error!( "{}: tls proxy error: received encrypted message from HU before TLS handshake", get_name());
                    }
                    else {
                        match msg.decrypt_payload(&mut mem_buf_hu, &mut server).await {
                            Ok(_) => {
                                let _ = self.pkt_debug(sdr_msg_types.get(&ch_id_hu).copied().unwrap_or(AAMessageType::Unknown),HexdumpLevel::DecryptedInput, self.dmp_level, &msg, "HU".parse().unwrap()).await;
                                let message_id: i32 = u16::from_be_bytes(msg.payload[0..=1].try_into()?).into();
                                if (msg.channel == 0) && (message_id ==ControlMessageType::MESSAGE_SERVICE_DISCOVERY_RESPONSE as i32)
                                {
                                    let data = &msg.payload[2..]; // start of message data, without message_id
                                     if  let Ok(_sdr) = ServiceDiscoveryResponse::parse_from_bytes(&data){
                                        for (_,proto_srv) in _sdr.services.iter().enumerate() {
                                            let sid=i32::from(proto_srv.id());
                                            if proto_srv.media_sink_service.is_some()
                                            {
                                                sdr_msg_types.insert(sid,AAMessageType::Media);
                                            }
                                            else if proto_srv.sensor_source_service.is_some()
                                            {
                                                sdr_msg_types.insert(sid,AAMessageType::Sensor);
                                            }
                                            else if proto_srv.media_source_service.is_some()
                                            {
                                                sdr_msg_types.insert(sid,AAMessageType::Media);
                                            }
                                            else if proto_srv.input_source_service.is_some()
                                            {
                                                sdr_msg_types.insert(sid,AAMessageType::Input);
                                            }
                                            else if proto_srv.bluetooth_service.is_some()
                                            {
                                                sdr_msg_types.insert(sid,AAMessageType::Bluetooth);
                                            }
                                            else if proto_srv.vendor_extension_service.is_some()
                                            {
                                                sdr_msg_types.insert(sid,AAMessageType::VendorExtension);
                                            }
                                            else if proto_srv.wifi_projection_service.is_some()
                                            {
                                                sdr_msg_types.insert(sid,AAMessageType::WiFiProjection);
                                            }
                                        }
                                        sdr=Some(_sdr);
                                     }
                                }
                                self.pkt_modify_hook(&mut msg, sdr_msg_types.get(&ch_id_hu).copied().unwrap_or(AAMessageType::Unknown)).await;
                                match msg.encrypt_payload(&mut mem_buf_md, &mut client).await {
                                Ok(_) => {
                                    msg.transmit(&mut md_tx).await.with_context(|| format!("{}: Service transmit to MD failed", get_name()))?;
                                }
                                Err(e) => {error!( "{} encrypt_payload error: {:?}", get_name(), e);},
                                }
                            }
                            Err(e) => {error!( "{} decrypt_payload error: {:?}", get_name(), e);},
                        }
                    }
                }
                else
                {
                    //let _ = self.pkt_debug(HexdumpLevel::DecryptedInput, self.dmp_level, &msg, "HU".parse().unwrap()).await;
                    // message_id is the first 2 bytes of payload
                    let message_id: i32 = u16::from_be_bytes(msg.payload[0..=1].try_into()?).into();
                    if !ssl_handshake_done && (message_id == ControlMessageType::MESSAGE_ENCAPSULATED_SSL as i32)
                    {
                        // doing SSL handshake
                            //Step1 MD: Send client hello
                            self.ssl_check_failure(client.do_handshake())?;
                            info!(
                                "{} 🔒 stage #{} of {}: MD SSL handshake: {}",
                                get_name(),
                                1,
                                3,
                                client.ssl().state_string_long(),
                            );
                            let pkt = self.ssl_encapsulate(mem_buf_md.clone()).await?;
                            //let _ = self.pkt_debug(HexdumpLevel::RawInput, self.dmp_level, &pkt, "MD".parse().unwrap()).await;
                            self.r_statistics.fetch_add(HEADER_LENGTH + pkt.payload.len(), Ordering::Relaxed);
                            pkt.transmit(&mut md_tx).await.with_context(|| format!("{}: transmit failed", get_name()))?;


                            //Step2 MD: Read server hello
                            info!("{} 🔒 MD reading server hello",get_name());
                            let pkt = md_rx.recv().await.ok_or("reader channel hung up")?;
                            //let _ = self.pkt_debug(HexdumpLevel::RawInput, self.dmp_level, &pkt, "MD".parse().unwrap()).await;
                            pkt.ssl_decapsulate_write(&mut mem_buf_md).await?;

                            //Step1 HU: parse client hello
                            //let _ = self.pkt_debug(HexdumpLevel::RawInput, self.dmp_level, &msg, "HU".parse().unwrap()).await;
                            msg.ssl_decapsulate_write(&mut mem_buf_hu).await?;
                            self.ssl_check_failure(server.accept())?;
                            info!(
                                "{} 🔒 stage #{} of {}: HU SSL handshake: {}",
                                get_name(),
                                1,
                                2,
                                server.ssl().state_string_long(),
                            );

                            //Step3 MD:
                            self.ssl_check_failure(client.do_handshake())?;
                            info!(
                                "{} 🔒 stage #{} of {}: MD SSL handshake: {}",
                                get_name(),
                                2,
                                3,
                                client.ssl().state_string_long(),
                            );
                            let pkt = self.ssl_encapsulate(mem_buf_md.clone()).await?;
                            //let _ = self.pkt_debug(HexdumpLevel::RawInput, self.dmp_level, &pkt, "MD".parse().unwrap()).await;
                            self.r_statistics.fetch_add(HEADER_LENGTH + pkt.payload.len(), Ordering::Relaxed);
                            pkt.transmit(&mut md_tx).await.with_context(|| format!("{}: transmit failed", get_name()))?;

                            // Step2 HU: send server hello
                            let pkt = self.ssl_encapsulate(mem_buf_hu.clone()).await?;
                            //let _ = self.pkt_debug(HexdumpLevel::RawOutput, self.dmp_level, &pkt,"HU".parse().unwrap()).await;
                            self.w_statistics.fetch_add(HEADER_LENGTH + pkt.payload.len(), Ordering::Relaxed);
                            pkt.transmit(&mut hu_wr).await.with_context(|| format!("{}: transmit failed", get_name()))?;

                            //Step4 MD:
                            let pkt = md_rx.recv().await.ok_or("reader channel hung up")?;
                            //let _ = self.pkt_debug(HexdumpLevel::RawInput, self.dmp_level, &pkt, "MD".parse().unwrap()).await;
                            pkt.ssl_decapsulate_write(&mut mem_buf_md).await?;

                            //Step3 HU: ClientKeyExchange
                            let pkt = hu_rx.recv().await.ok_or("hu reader channel hung up")?;
                            //let _ = self.pkt_debug(HexdumpLevel::RawInput, self.dmp_level, &pkt, "HU".parse().unwrap()).await;
                            pkt.ssl_decapsulate_write(&mut mem_buf_hu).await?;
                            self.ssl_check_failure(server.accept())?;
                            info!(
                                "{} 🔒 stage #{} of {}: HU SSL handshake: {}",
                                get_name(),
                                2,
                                2,
                                server.ssl().state_string_long(),
                            );
                            if server.ssl().is_init_finished() {
                                info!(
                                    "{} 🔒 HU SSL init complete, negotiated cipher: <b><blue>{}</>",
                                    get_name(),
                                    server.ssl().current_cipher().unwrap().name(),
                                );
                            }

                            //Step5 MD:
                            self.ssl_check_failure(client.do_handshake())?;
                            info!(
                                "{} 🔒 stage #{} of {}: MD SSL handshake: {}",
                                get_name(),
                                3,
                                3,
                                client.ssl().state_string_long(),
                            );
                            if client.ssl().is_init_finished() {
                                ssl_handshake_done=true;
                                info!(
                                    "{} 🔒 MD SSL init complete, negotiated cipher: <b><blue>{}</>",
                                    get_name(),
                                    client.ssl().current_cipher().unwrap().name(),
                                );
                            }
                            let pkt = self.ssl_encapsulate(mem_buf_md.clone()).await?;
                            //let _ = self.pkt_debug(AAMessageType::Control,HexdumpLevel::RawInput, self.dmp_level, &pkt, "MD".parse().unwrap()).await;
                            self.r_statistics.fetch_add(HEADER_LENGTH + pkt.payload.len(), Ordering::Relaxed);
                            pkt.transmit(&mut md_tx).await.with_context(|| format!("{}: transmit failed", get_name()))?;

                            //Step4 HU: Change Cipher spec finished
                            let pkt = self.ssl_encapsulate(mem_buf_hu.clone()).await?;
                            //let _ = self.pkt_debug(HexdumpLevel::RawOutput, self.dmp_level, &pkt, "HU".parse().unwrap()).await;
                            self.w_statistics.fetch_add(HEADER_LENGTH + pkt.payload.len(), Ordering::Relaxed);
                            pkt.transmit(&mut hu_wr).await.with_context(|| format!("{}: transmit failed", get_name()))?;

                            debug!("{} SSL sequence for MD and HU complete",get_name());
                    }
                    else
                    {
                        let _ = self.pkt_debug(AAMessageType::Control,HexdumpLevel::DecryptedInput, self.dmp_level, &msg, "HU".parse().unwrap()).await;
                        msg.transmit(&mut md_tx).await.with_context(|| format!("{}: Service transmit to MD failed", get_name()))?;
                    }
                }
            }
            //lower priority MD>HU
            Some(mut msg) = md_rx.recv() => {
                     // Increment byte counters for statistics
                    // fixme: compute final_len for precise stats
                    self.w_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                    //debug!("{}: Received {:?} bytes from MD", get_name(), HEADER_LENGTH + msg.payload.len());
                    if msg.flags&ENCRYPTED !=0
                    {
                        if !ssl_handshake_done
                        {
                                error!( "{}: tls proxy error: received encrypted message from service before TLS handshake", get_name());
                        }
                        else {
                               /* let _ = pkt_debug(
                                    HexdumpLevel::DecryptedOutput,
                                    dmp_level,
                                    &msg,
                                    "MD".parse().unwrap()
                                ).await;*/
                                match msg.decrypt_payload(&mut mem_buf_md, &mut client).await {
                                Ok(_) => {
                                    let _ = self.pkt_debug(sdr_msg_types.get(&(msg.channel as i32)).copied().unwrap_or(AAMessageType::Unknown),HexdumpLevel::DecryptedInput, self.dmp_level, &msg, "MD".parse().unwrap()).await;
                                    match msg.encrypt_payload(&mut mem_buf_hu, &mut server).await {
                                    Ok(_) => {
                                        msg.transmit(&mut hu_wr).await.with_context(|| format!("{}: Service transmit to HU failed", get_name()))?;
                                    }
                                    Err(e) => {error!( "{} encrypt_payload error: {:?}", get_name(), e);},
                                    }
                                }
                                Err(e) => {error!( "{} decrypt_payload error: {:?}", get_name(), e);},
                            }
                        }
                    }
                    else
                    {
                           let _ = self.pkt_debug(AAMessageType::Control,HexdumpLevel::DecryptedInput, self.dmp_level, &msg, "MD".parse().unwrap()).await;
                            msg.transmit(&mut hu_wr).await.with_context(|| format!("{}: Service transmit to HU failed", get_name()))?;
                    }
            }
            else => {
                // all channels closed
                tokio::time::sleep(Duration::from_secs(1)).await;
                error!("packet_tls_proxy ALL CHANNELS CLOSED! handle app restart needed")
                }
            }
        }

        Ok(())
    }

    async fn run_aa_pt<A: Endpoint<A>>(mut self, mut hu_wr: IoDevice<A>,
                                       mut hu_rx: Receiver<Packet>,
                                       mut md_rx: Receiver<Packet>,
                                       mut md_tx: IoDevice<TcpStream>,
    ) -> Result<()> {

        info!( "{}: Starting AA PT message proxy loop...", get_name());
        loop {
            tokio::select! {
            biased;

            // 🔴 highest priority, HU>MD
            Some(mut msg) = hu_rx.recv() => {
                    // Increment byte counters for statistics
                    // fixme: compute final_len for precise stats
                    self.r_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                    msg.transmit(&mut md_tx).await.with_context(|| format!("{}: Service transmit to MD failed", get_name()))?;
            }
            //lower priority MD>HU
            Some(mut msg) = md_rx.recv() => {
                    // Increment byte counters for statistics
                    // fixme: compute final_len for precise stats
                    self.w_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                    msg.transmit(&mut hu_wr).await.with_context(|| format!("{}: Service transmit to HU failed", get_name()))?;
            }
            else => {
                // all channels closed
                tokio::time::sleep(Duration::from_secs(1)).await;
                error!("packet_tls_proxy ALL CHANNELS CLOSED! handle app restart needed")
            }
            }
        }

        Ok(())
    }

    async fn run_mirror<A: Endpoint<A>>(mut self, mut hu_wr: IoDevice<A>,
                                        mut hu_rx: Receiver<Packet>,
                                        mut srv_rx: Receiver<Packet>,
                                        srv_tx: Sender<Packet>,
    ) -> Result<()> {
        let ssl = self.ssl_builder_md().await?;
        let mut mem_buf = SslMemBuf {
            client_stream: Arc::new(Mutex::new(VecDeque::new())),
            server_stream: Arc::new(Mutex::new(VecDeque::new())),
        };
        let mut ssl_handshake_done=false;
        let mut server = openssl::ssl::SslStream::new(ssl, mem_buf.clone())?;
        //Dump all remaining messages
        /*while srv_rx.try_recv().is_ok() {
        }*/
        info!( "{}: Starting MIRROR mode message proxy loop...", get_name());
        loop {
            tokio::select! {
            biased;

            // 🔴 highest priority, SCRCPY/SRV_CH>HU
            Some(mut msg) = srv_rx.recv() =>{
                    if msg.flags&ENCRYPTED !=0
                    {
                        if !ssl_handshake_done
                        {
                            error!( "{}: tls proxy error: received encrypted message from service before TLS handshake", get_name());
                        }
                        else
                        {
                            match msg.encrypt_payload(&mut mem_buf, &mut server).await {
                                Ok(_) => {
                                    // Increment byte counters for statistics
                                    // fixme: compute final_len for precise stats
                                    self.w_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                                    if msg.payload.len() > MAX_PACKET_LEN {
                                        error!("tls_proxy SRV>HU packet payload too big, got {}",msg.payload.len());
                                    }
                                    if let Err(e) = msg.transmit(&mut hu_wr).await.with_context(|| format!("{}: SRV transmit to HU failed", get_name())) {
                                        error!("SRV>HU Transmission error: {:?}", e);
                                        return Err(Box::new(io::Error::new(io::ErrorKind::Other, "SRV>HU channel closed")));
                                    }
                                    // yield so other tasks can run to release backpressure on TCP, this improves lag
                                    //tokio::task::yield_now().await;
                                }
                                Err(e) => {
                                    error!( "{} encrypt_payload error: {:?}", get_name(), e);
                                    return Err(Box::new(io::Error::new(io::ErrorKind::Other, "SRV>HU encrypt_payload error")));
                                },
                            }
                        }
                    }
                    else
                    {
                        self.w_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                        msg.transmit(&mut hu_wr).await.with_context(|| format!("{}: Service transmit to HU failed", get_name()))?;
                    }
            }
            // low priority, HU>Service/SCRCPY
            Some(mut msg) = hu_rx.recv() => {
                // Increment byte counters for statistics
                // fixme: compute final_len for precise stats
                self.r_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);

                if msg.flags&ENCRYPTED !=0
                {
                    if !ssl_handshake_done
                    {
                        error!( "{}: tls proxy error: received encrypted message from HU before TLS handshake", get_name());
                    }
                    else {
                        match msg.decrypt_payload(&mut mem_buf, &mut server).await {
                            Ok(_) => {
                                //check if is media ack message
                                if (self.audio_sid >0) && (self.video_sid>0) && ((msg.channel == self.audio_sid)||(msg.channel == self.video_sid))
                                {
                                    if !self.cfg.ignore_media_ack
                                    {
                                        let message_id: i32 = u16::from_be_bytes(msg.payload[0..=1].try_into()?).into();
                                        if message_id == MediaMessageId::MEDIA_MESSAGE_ACK as i32
                                        {
                                            if msg.channel == self.audio_sid
                                            {
                                                if let Some(ref mut scrcpy_tx)=self.audio_ack_rx
                                                {
                                                    scrcpy_tx.try_recv();
                                                    continue;
                                                }
                                                else
                                                {
                                                    error!( "{}: Media ACK error, audio_ack_rx is None", get_name());
                                                }
                                            }
                                            else if msg.channel == self.video_sid
                                            {
                                                if let Some(ref mut scrcpy_tx)=self.video_ack_rx
                                                {
                                                    scrcpy_tx.try_recv();
                                                    continue;
                                                }
                                                else
                                                {
                                                    error!( "{}: Media ACK error, video_ack_rx is None", get_name());
                                                }
                                            }
                                            else
                                            {
                                                error!( "{}: Media ACK unmanaged", get_name());
                                            }
                                        }
                                    }
                                }
                                else
                                {
                                    if let Err(_) = srv_tx.send(msg).await{
                                            error!( "{} tls proxy send to service error",get_name());
                                    }
                                }
                            }
                            Err(e) => {error!( "{} decrypt_payload error: {:?}", get_name(), e);},
                        }
                    }
                }
                else
                {
                    let _ = pkt_debug(HexdumpLevel::DecryptedInput, self.dmp_level, &msg, "HU".parse().unwrap()).await;
                    // message_id is the first 2 bytes of payload
                    let message_id: i32 = u16::from_be_bytes(msg.payload[0..=1].try_into()?).into();
                    if !ssl_handshake_done && (message_id == ControlMessageType::MESSAGE_ENCAPSULATED_SSL as i32)
                    {
                        // doing SSL handshake
                            //Step1: parse client hello
                            let _ = pkt_debug(HexdumpLevel::RawInput, self.dmp_level, &msg, "HU".parse().unwrap()).await;
                            msg.ssl_decapsulate_write(&mut mem_buf).await?;
                            self.ssl_check_failure(server.accept())?;
                            info!(
                                "{} 🔒 stage #{} of {}: SSL handshake: {}",
                                get_name(),
                                1,
                                2,
                                server.ssl().state_string_long(),
                            );
                            // Step2: send server hello
                            let pkt = self.ssl_encapsulate(mem_buf.clone()).await?;
                            let _ = pkt_debug(HexdumpLevel::RawOutput, self.dmp_level, &pkt,"MD".parse().unwrap()).await;
                            pkt.transmit(&mut hu_wr).await.with_context(|| format!("{}: transmit failed", get_name()))?;

                            //Step3: ClientKeyExchange
                            let pkt = hu_rx.recv().await.ok_or("hu reader channel hung up")?;
                            let _ = pkt_debug(HexdumpLevel::RawInput, self.dmp_level, &pkt, "HU".parse().unwrap()).await;
                            pkt.ssl_decapsulate_write(&mut mem_buf).await?;
                            self.ssl_check_failure(server.accept())?;
                            info!(
                                "{} 🔒 stage #{} of {}: SSL handshake: {}",
                                get_name(),
                                2,
                                2,
                                server.ssl().state_string_long(),
                            );
                            if server.ssl().is_init_finished() {
                                ssl_handshake_done=true;
                                info!(
                                    "{} 🔒 SSL init complete, negotiated cipher: <b><blue>{}</>",
                                    get_name(),
                                    server.ssl().current_cipher().unwrap().name(),
                                );
                            }
                            //Step4: Change Cipher spec finished
                            let pkt = self.ssl_encapsulate(mem_buf.clone()).await?;
                            let _ = pkt_debug(HexdumpLevel::RawOutput, self.dmp_level, &pkt, "MD".parse().unwrap()).await;
                            pkt.transmit(&mut hu_wr).await.with_context(|| format!("{}: transmit failed", get_name()))?;
                    }
                    else {
                        if let Err(_) = srv_tx.send(msg).await{
                            error!( "{} tls proxy send to service error",get_name());
                        }
                    }

                }
            }
            else => {
                // all channels closed
                tokio::time::sleep(Duration::from_secs(1)).await;
                error!("packet_tls_proxy ALL CHANNELS CLOSED! handle app restart needed")
            }
            }
        }

        Ok(())
    }
    pub fn start<A: Endpoint<A> + 'static>(self, hu_wr: IoDevice<A>,
                                           hu_rx: Receiver<Packet>,
                                           md_rx: Receiver<Packet>,
                                           md_tx: Option<IoDevice<TcpStream>>,
                                           srv_tx: Option<Sender<Packet>>,
    ) -> Result<JoinHandle<Result<()>>> {
        if self.cfg.aa_mode == AAMode::PassThrough
        {
            let md_tx = md_tx.ok_or_else(|| anyhow!("md_tx is NONE"))?;
            if self.cfg.mitm
            {
                Ok(tokio_uring::spawn(async move {
                    self.run_aa_mitm(hu_wr, hu_rx, md_rx, md_tx).await
                }))
            }
            else
            {
                Ok(tokio_uring::spawn(async move {
                    self.run_aa_pt(hu_wr, hu_rx, md_rx, md_tx).await
                }))
            }
        }
        else {
            let srv_tx = srv_tx.ok_or_else(|| anyhow!("srv_tx is NONE"))?;
            Ok(tokio_uring::spawn(async move {
                self.run_mirror(hu_wr, hu_rx, md_rx, srv_tx).await
            }))
        }

    }
    /// packet modification hook for AA MITM mode only
    async fn pkt_modify_hook(&self, pkt: &mut Packet, pkt_type:AAMessageType) {
        if pkt_type == AAMessageType::Control
        {
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into().unwrap()).into();
            if message_id ==ControlMessageType::MESSAGE_SERVICE_DISCOVERY_RESPONSE as i32
            {
                if self.cfg.developer_mode || self.cfg.dpi > 0
                {
                    let data = &pkt.payload[2..]; // start of message data
                    if let Ok(mut msg) = ServiceDiscoveryResponse::parse_from_bytes(&data){
                        if self.cfg.developer_mode
                        {
                            msg.set_make(DHU_MAKE_DEV.into());
                            msg.set_model(DHU_MODEL_DEV.into());
                            msg.set_head_unit_make(DHU_MAKE_DEV.into());
                            msg.set_head_unit_model(DHU_MODEL_DEV.into());
                            if let Some(info) = msg.headunit_info.as_mut() {
                                info.set_make(DHU_MAKE_DEV.into());
                                info.set_model(DHU_MODEL_DEV.into());
                                info.set_head_unit_make(DHU_MAKE_DEV.into());
                                info.set_head_unit_model(DHU_MODEL_DEV.into());
                            }
                            info!("{}/packet_modify_hook: <yellow>enabling developer mode</>",get_name());
                        }
                        if self.cfg.dpi > 0
                        {
                            if let Some(svc) = msg
                                .services
                                .iter_mut()
                                .find(|svc| !svc.media_sink_service.video_configs.is_empty())
                            {
                                // get previous/original value
                                let prev_val = svc.media_sink_service.video_configs[0].density();
                                // set new value
                                svc.media_sink_service.as_mut().unwrap().video_configs[0]
                                    .set_density(self.cfg.dpi.into());
                                info!("{}/packet_modify_hook: <yellow>replacing DPI value from <b>{}</> to <b>{}</></>",get_name(),prev_val,self.cfg.dpi);
                            }
                        }

                        // Regenerate payload with ALL spoofed fields
                        pkt.payload = msg.write_to_bytes().expect("error regenerating Packet payload");
                        pkt.payload.insert(0, (message_id >> 8) as u8);
                        pkt.payload.insert(1, (message_id & 0xff) as u8);
                    }
                }
            }
        }
        else if pkt_type == AAMessageType::Sensor
        {
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into().unwrap()).into();
            if message_id ==SensorMessageId::SENSOR_MESSAGE_BATCH as i32
            {
                if self.cfg.park_mode
                {
                    let data = &pkt.payload[2..]; // start of message data
                    if let Ok(mut msg) = SensorBatch::parse_from_bytes(&data) {
                        // === DRIVING STATUS: must be UNRESTRICTED (0) ===
                        // This is the primary flag AA checks. Value is a bitmask:
                        // 0 = unrestricted, 1 = no video, 2 = no keyboard, etc.
                        if !msg.driving_status_data.is_empty() {
                            msg.driving_status_data[0].set_status(0);
                        }
                        // === GEAR: force PARK ===
                        if !msg.gear_data.is_empty() {
                            msg.gear_data[0].set_gear(Gear::GEAR_PARK);
                        }

                        // === PARKING BRAKE: engaged ===
                        // Modern AA cross-checks parking brake with gear/speed.
                        if !msg.parking_brake_data.is_empty() {
                            msg.parking_brake_data[0].set_parking_brake(true);
                        }

                        // === VEHICLE SPEED: zero ===
                        // SpeedData.speed_e3 is speed in m/s * 1000. Zero = stopped.
                        /*if !msg.speed_data.is_empty() {
                            msg.speed_data[0].set_speed_e3(0);
                            // Also ensure cruise control is disengaged
                            msg.speed_data[0].set_cruise_engaged(false);
                        }*/

                        // === GPS/LOCATION: zero speed, keep position ===
                        // LocationData.speed_e3 is GPS-derived speed.
                        // Modern AA compares this against SpeedData for consistency.
                        /*if !msg.location_data.is_empty() {
                            msg.location_data[0].set_speed_e3(0);
                            // Zero bearing = not turning
                            msg.location_data[0].set_bearing_e6(0);
                        }*/

                        // === ACCELEROMETER: gravity only (stationary) ===
                        // A parked car only feels gravity on Z axis (~9810 mm/s²).
                        // Any X/Y acceleration implies movement/turning.
                        if !msg.accelerometer_data.is_empty() {
                            msg.accelerometer_data[0].set_acceleration_x_e3(0);
                            msg.accelerometer_data[0].set_acceleration_y_e3(0);
                            msg.accelerometer_data[0].set_acceleration_z_e3(9810);
                        }

                        // === GYROSCOPE: zero rotation ===
                        // Any rotation speed implies the vehicle is turning.
                        if !msg.gyroscope_data.is_empty() {
                            msg.gyroscope_data[0].set_rotation_speed_x_e3(0);
                            msg.gyroscope_data[0].set_rotation_speed_y_e3(0);
                            msg.gyroscope_data[0].set_rotation_speed_z_e3(0);
                        }

                        // === DEAD RECKONING: zero wheel speed + steering ===
                        // Wheel speed ticks and steering angle are used by Toyota
                        // and other modern HUs as independent motion verification.
                        if !msg.dead_reckoning_data.is_empty() {
                            msg.dead_reckoning_data[0].set_steering_angle_e1(0);
                            msg.dead_reckoning_data[0].wheel_speed_e3.clear();
                            // Push four zero values for the four wheels
                            msg.dead_reckoning_data[0].wheel_speed_e3.push(0);
                            msg.dead_reckoning_data[0].wheel_speed_e3.push(0);
                            msg.dead_reckoning_data[0].wheel_speed_e3.push(0);
                            msg.dead_reckoning_data[0].wheel_speed_e3.push(0);
                        }

                        // === COMPASS: freeze bearing ===
                        // Changing compass bearing implies turning/moving.
                        if !msg.compass_data.is_empty() {
                            msg.compass_data[0].set_pitch_e6(0);
                            msg.compass_data[0].set_roll_e6(0);
                        }

                        // === RPM: idle engine ===
                        // High RPM with zero speed is suspicious on some HUs.
                        // ~800 RPM idle is realistic for a parked car.
                        /*if !msg.rpm_data.is_empty() {
                            msg.rpm_data[0].set_rpm_e3(800_000);
                        }*/

                        // Regenerate payload with ALL spoofed fields
                        pkt.payload = msg.write_to_bytes().expect("error regenerating Packet payload");
                        pkt.payload.insert(0, (message_id >> 8) as u8);
                        pkt.payload.insert(1, (message_id & 0xff) as u8);
                    }
                }
            }
        }
    }

    /// creates Ssl for MobileDevice (SSL client)
    async fn ssl_builder_md(&self) -> Result<Ssl> {
        let mut ctx_builder = SslContextBuilder::new(SslMethod::tls())?;

        // for HU/headunit we need to act as a MD/mobiledevice, so load "md" key and cert
        ctx_builder.set_certificate_file(format!("{KEYS_PATH}/md_cert.pem"), SslFiletype::PEM)?;
        ctx_builder.set_private_key_file(format!("{KEYS_PATH}/md_key.pem"), SslFiletype::PEM)?;
        ctx_builder.check_private_key()?;
        // trusted root certificates:
        ctx_builder.set_ca_file(format!("{KEYS_PATH}/galroot_cert.pem"))?;

        ctx_builder.set_min_proto_version(Some(openssl::ssl::SslVersion::TLS1_2))?;
        ctx_builder.set_options(openssl::ssl::SslOptions::NO_TLSV1_3);

        let openssl_ctx = ctx_builder.build();
        let mut ssl = Ssl::new(&openssl_ctx)?;
        ssl.set_accept_state(); // SSL server
        Ok(ssl)
    }

    /// creates Ssl for HeadUnit (SSL server)
    async fn ssl_builder_hu(&self) -> Result<Ssl> {
        let mut ctx_builder = SslContextBuilder::new(SslMethod::tls())?;

        // for MD we need to act as a HU, so load "hu" key and cert
        ctx_builder.set_certificate_file(format!("{KEYS_PATH}/hu_cert.pem"), SslFiletype::PEM)?;
        ctx_builder.set_private_key_file(format!("{KEYS_PATH}/hu_key.pem"), SslFiletype::PEM)?;
        ctx_builder.check_private_key()?;
        // trusted root certificates:
        ctx_builder.set_ca_file(format!("{KEYS_PATH}/galroot_cert.pem"))?;

        ctx_builder.set_min_proto_version(Some(openssl::ssl::SslVersion::TLS1_2))?;
        ctx_builder.set_options(openssl::ssl::SslOptions::NO_TLSV1_3);

        let openssl_ctx = ctx_builder.build();
        let mut ssl = Ssl::new(&openssl_ctx)?;
        ssl.set_connect_state(); // SSL client
        Ok(ssl)
    }

    /// checking if there was a true fatal SSL error
    /// Note that the error may not be fatal. For example if the underlying
    /// stream is an asynchronous one then `HandshakeError::WouldBlock` may
    /// just mean to wait for more I/O to happen later.
    fn ssl_check_failure<T>(&self, res: std::result::Result<T, openssl::ssl::Error>) -> Result<()> {
        if let Err(err) = res {
            match err.code() {
                ErrorCode::WANT_READ | ErrorCode::WANT_WRITE | ErrorCode::SYSCALL => Ok(()),
                _ => return Err(Box::new(err)),
            }
        } else {
            Ok(())
        }
    }

    /// encapsulates SSL data into Packet
    async fn ssl_encapsulate(&self, mut mem_buf: SslMemBuf) -> Result<Packet> {
        // read SSL-generated data
        let mut res: Vec<u8> = Vec::new();
        mem_buf.read_to(&mut res)?;

        // create MESSAGE_ENCAPSULATED_SSL Packet
        let message_type = ControlMessageType::MESSAGE_ENCAPSULATED_SSL as u16;
        res.insert(0, (message_type >> 8) as u8);
        res.insert(1, (message_type & 0xff) as u8);
        Ok(Packet {
            channel: 0x00,
            flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: res,
        })
    }

    /// shows packet/message contents as pretty string for debug
    pub async fn pkt_debug(&self,
                           msg_type:AAMessageType,
                           hexdump: HexdumpLevel,
                           hex_requested: HexdumpLevel,
                           pkt: &Packet,
                           source:String,
    ) -> Result<()> {
        // don't run further if we are not in Debug mode
        if !log_enabled!(Level::Debug) {
            return Ok(());
        }

        // if for some reason we have too small packet, bail out
        if pkt.payload.len() < 2 {
            return Ok(());
        }
        // message_id is the first 2 bytes of payload
        let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();


        //debug!("{}> ch: {} flags: {:04X} message_id = {:04X}",source, pkt.channel,pkt.flags, message_id);
        if hex_requested >= hexdump {
            debug!("{} {:?} {}", get_name(), hexdump, pkt);
        }

        // parsing data
        let data = &pkt.payload[2..]; // start of message data
        match msg_type {
            AAMessageType::Control =>
            {
                // trying to obtain an Enum from message_id
                let control = match protos::ControlMessageType::from_i32(message_id) {
                    Some(c) => c,
                    None => return Ok(()),
                };
                let message: &dyn MessageDyn = match control {
                    ControlMessageType::MESSAGE_VERSION_REQUEST => &VersionRequest::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_BYEBYE_REQUEST => &ByeByeRequest::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_BYEBYE_RESPONSE => &ByeByeResponse::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_AUTH_COMPLETE => &AuthResponse::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_SERVICE_DISCOVERY_REQUEST => &ServiceDiscoveryRequest::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_SERVICE_DISCOVERY_RESPONSE => &ServiceDiscoveryResponse::parse_from_bytes(data)?,
                    //ControlMessageType::MESSAGE_PING_REQUEST => &PingRequest::parse_from_bytes(data)?,
                    //ControlMessageType::MESSAGE_PING_RESPONSE => &PingResponse::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_NAV_FOCUS_REQUEST => &NavFocusRequestNotification::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_CHANNEL_OPEN_RESPONSE => &ChannelOpenResponse::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_CHANNEL_OPEN_REQUEST => &ChannelOpenRequest::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_AUDIO_FOCUS_REQUEST => &AudioFocusRequestNotification::parse_from_bytes(data)?,
                    ControlMessageType::MESSAGE_AUDIO_FOCUS_NOTIFICATION => &AudioFocusNotification::parse_from_bytes(data)?,
                    _ => return Ok(()),
                };
                // show pretty string from the message
                debug!("{}", print_to_string_pretty(message));
            }
            AAMessageType::Media =>
                {
                    // trying to obtain an Enum from message_id
                    let control = match protos::MediaMessageId::from_i32(message_id) {
                        Some(c) => c,
                        None => return Ok(()),
                    };
                    let message: &dyn MessageDyn = match control {
                        MediaMessageId::MEDIA_MESSAGE_SETUP =>&Setup::parse_from_bytes(data)?,
                        MediaMessageId::MEDIA_MESSAGE_START =>&Start::parse_from_bytes(data)?,
                        MediaMessageId::MEDIA_MESSAGE_CONFIG =>&ChConfig::parse_from_bytes(data)?,
                        //MediaMessageId::MEDIA_MESSAGE_ACK =>&Ack::parse_from_bytes(data)?,
                        MediaMessageId::MEDIA_MESSAGE_VIDEO_FOCUS_REQUEST =>&VideoFocusRequestNotification::parse_from_bytes(data)?,
                        MediaMessageId::MEDIA_MESSAGE_VIDEO_FOCUS_NOTIFICATION =>&VideoFocusNotification::parse_from_bytes(data)?,
                        _ => return Ok(()),
                    };
                    // show pretty string from the message
                    debug!("{}", print_to_string_pretty(message));
                }
            AAMessageType::Sensor =>
                {
                    // trying to obtain an Enum from message_id
                    let control = match protos::SensorMessageId::from_i32(message_id) {
                        Some(c) => c,
                        None => return Ok(()),
                    };
                    let message: &dyn MessageDyn = match control {
                        SensorMessageId::SENSOR_MESSAGE_REQUEST =>&SensorRequest::parse_from_bytes(data)?,
                        SensorMessageId::SENSOR_MESSAGE_RESPONSE =>&SensorResponse::parse_from_bytes(data)?,
                        SensorMessageId::SENSOR_MESSAGE_BATCH =>&SensorBatch::parse_from_bytes(data)?,
                        SensorMessageId::SENSOR_MESSAGE_ERROR =>&SensorError::parse_from_bytes(data)?,
                        _ => return Ok(()),
                    };
                    // show pretty string from the message
                    debug!("{}", print_to_string_pretty(message));
                }
            _ => {
                debug!("Unknown message type received: {:?}", msg_type);
            }
        }

        Ok(())
    }

    fn get_name(&self,) -> String {
        "PacketProxy".to_string()
    }
}

/// shows packet/message contents as pretty string for debug
pub async fn pkt_debug(
    hexdump: HexdumpLevel,
    hex_requested: HexdumpLevel,
    pkt: &Packet,
    source:String,
) -> Result<()> {
    // don't run further if we are not in Debug mode
    if !log_enabled!(Level::Debug) {
        return Ok(());
    }

    // if for some reason we have too small packet, bail out
    if pkt.payload.len() < 2 {
        return Ok(());
    }
    // message_id is the first 2 bytes of payload
    let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();

    // trying to obtain an Enum from message_id
    let control = protos::ControlMessageType::from_i32(message_id);
    debug!("{}> ch: {} flags: {:04X} message_id = {:04X}, {:?}",source, pkt.channel,pkt.flags, message_id, control);
    if hex_requested >= hexdump {
        debug!("{} {:?} {}", get_name(), hexdump, pkt);
    }

    // parsing data
    let data = &pkt.payload[2..]; // start of message data
    let message: &dyn MessageDyn = match control.unwrap() {
        ControlMessageType::MESSAGE_VERSION_REQUEST => &VersionRequest::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_BYEBYE_REQUEST => &ByeByeRequest::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_BYEBYE_RESPONSE => &ByeByeResponse::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_AUTH_COMPLETE => &AuthResponse::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_SERVICE_DISCOVERY_REQUEST => &ServiceDiscoveryRequest::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_SERVICE_DISCOVERY_RESPONSE => &ServiceDiscoveryResponse::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_PING_REQUEST => &PingRequest::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_PING_RESPONSE => &PingResponse::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_NAV_FOCUS_REQUEST => &NavFocusRequestNotification::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_CHANNEL_OPEN_RESPONSE => &ChannelOpenResponse::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_CHANNEL_OPEN_REQUEST => &ChannelOpenRequest::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_AUDIO_FOCUS_REQUEST => &AudioFocusRequestNotification::parse_from_bytes(data)?,
        ControlMessageType::MESSAGE_AUDIO_FOCUS_NOTIFICATION => &AudioFocusNotification::parse_from_bytes(data)?,
        _ => return Ok(()),
    };
    // show pretty string from the message
    debug!("{}", print_to_string_pretty(message));

    Ok(())
}

/// reads all available data to VecDeque
async fn read_input_data<A: Endpoint<A>>(
    rbuf: &mut VecDeque<u8>,
    obj: &mut IoDevice<A>,
) -> Result<usize> {
    let mut newdata = vec![0u8; BUFFER_LEN];
    let n;
    let len;

    match obj {
        IoDevice::UsbReader(device, _) => {
            let mut dev = device.borrow_mut();
            let retval = dev.read(&mut newdata);
            len = retval
                .await
                .context("read_input_data: UsbReader read error")?;
        }
        IoDevice::EndpointIo(device) => {
            let retval = device.read(newdata);
            (n, newdata) = timeout(Duration::from_millis(15000), retval)
                .await
                .context("read_input_data: EndpointIo timeout")?;
            len = n.context("read_input_data: EndpointIo read error")?;
        }
        IoDevice::TcpStreamIo(device) => {
            let retval = device.read(newdata);
            (n, newdata) = timeout(Duration::from_millis(15000), retval)
                .await
                .context("read_input_data: TcpStreamIo timeout")?;
            len = n.context("read_input_data: TcpStreamIo read error")?;
            if len == 0 {
                // TCP EOF means the peer closed the connection; propagate as disconnect.
                return Err("read_input_data: TcpStreamIo EOF".into());
            }
        }
        _ => todo!(),
    }
    if len > 0 {
        rbuf.write(&newdata.slice(..len))?;
    }
    Ok((len))
}

/// main reader thread for a device
pub async fn endpoint_reader<A: Endpoint<A>>(
    mut device: IoDevice<A>,
    tx: Sender<Packet>,
) -> Result<()> {
    let mut rbuf: VecDeque<u8> = VecDeque::new();
    loop {
        read_input_data(&mut rbuf, &mut device).await?;
        // check if we have complete packet available
        loop {
            // Accept packets as soon as we have the complete fixed header.
            // Using >= is required for valid zero-payload frames (frame_size == HEADER_LENGTH).
            if rbuf.len() >= HEADER_LENGTH {
                let channel = rbuf[0];
                let flags = rbuf[1];

                // FIRST frames carry an extended 8-byte header. If only 4 bytes
                // are buffered, wait for the remaining header bytes before parsing.
                if (flags & FRAME_TYPE_MASK) == FRAME_TYPE_FIRST && rbuf.len() < 8 {
                    break;
                }

                let mut header_size = HEADER_LENGTH;
                let mut final_length = None;
                let payload_size = (rbuf[3] as u16 + ((rbuf[2] as u16) << 8)) as usize;
                if rbuf.len() >= 8 && (flags & FRAME_TYPE_MASK) == FRAME_TYPE_FIRST {
                    header_size += 4;
                    final_length = Some(
                        ((rbuf[4] as u32) << 24)
                            + ((rbuf[5] as u32) << 16)
                            + ((rbuf[6] as u32) << 8)
                            + (rbuf[7] as u32),
                    );
                }
                let frame_size = header_size + payload_size;
                if rbuf.len() >= frame_size {
                    let mut frame = vec![0u8; frame_size];
                    rbuf.read_exact(&mut frame)?;
                    // we now have all header data analyzed/read, so remove
                    // the header from frame to have payload only left
                    frame.drain(..header_size);
                    let pkt = Packet {
                        channel,
                        flags,
                        final_length,
                        payload: frame,
                    };
                    // send packet to main thread for further process
                    tx.send(pkt).await?;
                    // check if we have another packet
                    continue;
                }
            }
            // no more complete packets available
            break;
        }
    }
}
