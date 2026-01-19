use anyhow::Context;
use log::log_enabled;
use openssl::ssl::{ErrorCode, Ssl, SslContextBuilder, SslFiletype, SslMethod};
use simplelog::*;
use std::collections::VecDeque;
use std::{fmt};
use std::cmp::PartialEq;
use std::ffi::CString;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use flume::TryRecvError;
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
use crate::channel_manager::ByeByeReason::USER_SELECTION;
use crate::channel_manager::MessageStatus;
use protobuf::text_format::print_to_string_pretty;
use protobuf::{Enum, Message, MessageDyn};
use tokio::sync::{mpsc};
use protos::ControlMessageType::{self, *};
use crate::aa_services::{VideoCodecResolution::*, VideoFPS::*, AudioStream::*, VideoConfig, AudioConfig, AudioChConfiguration, MediaCodec, MediaCodec::*, AudioStream, ServiceType, CommandState, ServiceStatus, th_bluetooth, VideoStreamingParams, AudioStreamingParams};
use crate::aa_services::{th_input_source, th_media_sink_audio_guidance, th_media_sink_audio_streaming, th_media_sink_video, th_media_source, th_sensor_source, th_vendor_extension};
use crate::config;
use crate::config::HU_CONFIG_DELAY_MS;
use crate::config_types::HexdumpLevel;
use crate::io_uring::Endpoint;
use crate::io_uring::IoDevice;
use crate::io_uring::BUFFER_LEN;

// module name for logging engine
fn get_name() -> String {
    let dev = "CH Manager";
    format!("<i><bright-black> aa-mirror/{}: </>", dev)
}

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

// message related constants:
pub const HEADER_LENGTH: usize = 4;
pub const FRAME_TYPE_FIRST: u8 = 1 << 0;
pub const FRAME_TYPE_LAST: u8 = 1 << 1;
pub const FRAME_TYPE_MASK: u8 = FRAME_TYPE_FIRST | FRAME_TYPE_LAST;
pub const FRAME_TYPE_CONTROL: u8 = 1 << 2;
pub const ENCRYPTED: u8 = 1 << 3;

// location for hu_/md_ private keys and certificates:
const KEYS_PATH: &str = "/etc/aa-mirror-rs";
const RES_PATH: &str = "/etc/aa-mirror-rs/res";

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
    async fn transmit<A: Endpoint<A>>(
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
    let message: &dyn MessageDyn = match control.unwrap_or(MESSAGE_UNEXPECTED_MESSAGE) {
        MESSAGE_VERSION_REQUEST => &VersionRequest::parse_from_bytes(data)?,
        MESSAGE_BYEBYE_REQUEST => &ByeByeRequest::parse_from_bytes(data)?,
        MESSAGE_BYEBYE_RESPONSE => &ByeByeResponse::parse_from_bytes(data)?,
        MESSAGE_AUTH_COMPLETE => &AuthResponse::parse_from_bytes(data)?,
        MESSAGE_SERVICE_DISCOVERY_REQUEST => &ServiceDiscoveryRequest::parse_from_bytes(data)?,
        MESSAGE_SERVICE_DISCOVERY_RESPONSE => &ServiceDiscoveryResponse::parse_from_bytes(data)?,
        MESSAGE_PING_REQUEST => &PingRequest::parse_from_bytes(data)?,
        MESSAGE_PING_RESPONSE => &PingResponse::parse_from_bytes(data)?,
        MESSAGE_NAV_FOCUS_REQUEST => &NavFocusRequestNotification::parse_from_bytes(data)?,
        MESSAGE_CHANNEL_OPEN_RESPONSE => &ChannelOpenResponse::parse_from_bytes(data)?,
        MESSAGE_CHANNEL_OPEN_REQUEST => &ChannelOpenRequest::parse_from_bytes(data)?,
        MESSAGE_AUDIO_FOCUS_REQUEST => &AudioFocusRequestNotification::parse_from_bytes(data)?,
        MESSAGE_AUDIO_FOCUS_NOTIFICATION => &AudioFocusNotification::parse_from_bytes(data)?,
        MEDIA_MESSAGE_SETUP =>&Setup::parse_from_bytes(data)?,
        MEDIA_MESSAGE_START =>&Start::parse_from_bytes(data)?,
        MEDIA_MESSAGE_CONFIG =>&ChConfig::parse_from_bytes(data)?,
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
) -> Result<()> {
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
        }
        _ => todo!(),
    }
    if len > 0 {
        rbuf.write(&newdata.slice(..len))?;
    }
    Ok(())
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
            if rbuf.len() > HEADER_LENGTH {
                let channel = rbuf[0];
                let flags = rbuf[1];
                let mut header_size = HEADER_LENGTH;
                let mut final_length = None;
                let payload_size = (rbuf[3] as u16 + ((rbuf[2] as u16) << 8)) as usize;
                if rbuf.len() > 8 && (flags & FRAME_TYPE_MASK) == FRAME_TYPE_FIRST {
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
                    info!("Channel {} received {} bytes from HU", channel ,payload_size);
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

/// main reader thread for a service
pub async fn packet_tls_proxy<A: Endpoint<A>>(
    mut hu_wr: IoDevice<A>,
    mut hu_rx: Receiver<Packet>,
    mut srv_rx: Receiver<Packet>,
    srv_tx: Sender<Packet>,
    mut scrcpy_rx: flume::Receiver<Packet>,
    r_statistics: Arc<AtomicUsize>,
    w_statistics: Arc<AtomicUsize>,
    dmp_level:HexdumpLevel,
    ) -> Result<()> {
    let mut ssl_handshake_done:bool=false;
    let mut hu_read_err:bool=false;
    let mut srv_read_err:bool=false;
    let ssl = ssl_builder().await?;
    let mut mem_buf = SslMemBuf {
        client_stream: Arc::new(Mutex::new(VecDeque::new())),
        server_stream: Arc::new(Mutex::new(VecDeque::new())),
    };
    let mut server = openssl::ssl::SslStream::new(ssl, mem_buf.clone())?;
    info!( "{}: Starting message proxy loop...", get_name());
    loop {
        //HU>Service
        match hu_rx.try_recv() {
            Ok(mut msg) => {
                hu_read_err=false;
                // Increment byte counters for statistics
                // fixme: compute final_len for precise stats
                r_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);

                if msg.flags&ENCRYPTED !=0
                {
                    if !ssl_handshake_done
                    {
                        error!( "{}: tls proxy error: received encrypted message from HU before TLS handshake", get_name());
                    }
                    else {
                        match msg.decrypt_payload(&mut mem_buf, &mut server).await {
                            Ok(_) => {
                                let _ = pkt_debug(
                                    HexdumpLevel::DecryptedInput,
                                    dmp_level,
                                    &msg,
                                    "HU".parse().unwrap()
                                ).await;
                                if let Err(_) = srv_tx.send(msg).await{
                                    error!( "{} tls proxy send to service error",get_name());
                                };
                            }
                            Err(e) => {error!( "{} decrypt_payload error: {:?}", get_name(), e);},
                        }
                    }
                }
                else
                {
                    let _ = pkt_debug(HexdumpLevel::DecryptedInput, dmp_level, &msg, "HU".parse().unwrap()).await;
                    // message_id is the first 2 bytes of payload
                    let message_id: i32 = u16::from_be_bytes(msg.payload[0..=1].try_into()?).into();
                    if !ssl_handshake_done && (protos::ControlMessageType::from_i32(message_id).unwrap_or(MESSAGE_UNEXPECTED_MESSAGE) == MESSAGE_ENCAPSULATED_SSL)
                    {
                        // doing SSL handshake
                            //Step1: parse client hello
                            let _ = pkt_debug(HexdumpLevel::RawInput, dmp_level, &msg, "HU".parse().unwrap()).await;
                            msg.ssl_decapsulate_write(&mut mem_buf).await?;
                            ssl_check_failure(server.accept())?;
                            info!(
                                "{} 🔒 stage #{} of {}: SSL handshake: {}",
                                get_name(),
                                1,
                                2,
                                server.ssl().state_string_long(),
                            );
                            // Step2: send server hello
                            let pkt = ssl_encapsulate(mem_buf.clone()).await?;
                            let _ = pkt_debug(HexdumpLevel::RawOutput, dmp_level, &pkt,"MD".parse().unwrap()).await;
                            pkt.transmit(&mut hu_wr).await.with_context(|| format!("{}: transmit failed", get_name()))?;

                            //Step3: ClientKeyExchange
                            let pkt = hu_rx.recv().await.ok_or("hu reader channel hung up")?;
                            let _ = pkt_debug(HexdumpLevel::RawInput, dmp_level, &pkt, "HU".parse().unwrap()).await;
                            pkt.ssl_decapsulate_write(&mut mem_buf).await?;
                            ssl_check_failure(server.accept())?;
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
                            let pkt = ssl_encapsulate(mem_buf.clone()).await?;
                            let _ = pkt_debug(HexdumpLevel::RawOutput, dmp_level, &pkt, "MD".parse().unwrap()).await;
                            pkt.transmit(&mut hu_wr).await.with_context(|| format!("{}: transmit failed", get_name()))?;
                    }
                    else {
                        if let Err(_) = srv_tx.send(msg).await{
                            error!( "{} tls proxy send to service error",get_name());
                        };
                    }

                }
            }

            // For both errors (Disconnected and Empty), the correct action
            // is to process the items.  If the error was Disconnected, on
            // the next iteration rx.recv().await will be None and we'll
            // break from the outer loop anyway.
            Err(_) => {
                /*error!( "{}: tls proxy error receiving message from HU", get_name());*/
                //thread::yield_now();
                hu_read_err=true;
            },
        }

        //Service>HU
        match srv_rx.try_recv() {
            Ok(mut msg) => {
                srv_read_err=false;
                if msg.flags&ENCRYPTED !=0
                {
                    if !ssl_handshake_done
                    {
                        error!( "{}: tls proxy error: received encrypted message from service before TLS handshake", get_name());
                    }
                    else {
                        let _ = pkt_debug(
                            HexdumpLevel::DecryptedOutput,
                            dmp_level,
                            &msg,
                            "MD".parse().unwrap()
                        ).await;
                        match msg.encrypt_payload(&mut mem_buf, &mut server).await {
                            Ok(_) => {
                                // Increment byte counters for statistics
                                // fixme: compute final_len for precise stats
                                w_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                                msg.transmit(&mut hu_wr).await.with_context(|| format!("{}: transmit to HU failed", get_name()))?;
                            }
                            Err(e) => {error!( "{} encrypt_payload error: {:?}", get_name(), e);},
                        }
                    }
                }
                else {
                    let _ = pkt_debug(
                        HexdumpLevel::DecryptedOutput,
                        dmp_level,
                        &msg,
                        "MD".parse().unwrap()
                    ).await;
                    // Increment byte counters for statistics
                    // fixme: compute final_len for precise stats
                    w_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                    msg.transmit(&mut hu_wr).await.with_context(|| format!("{}: transmit to HU failed", get_name()))?;

                }
            }

            // For both errors (Disconnected and Empty), the correct action
            // is to process the items.  If the error was Disconnected, on
            // the next iteration rx.recv().await will be None and we'll
            // break from the outer loop anyway.
            Err(_) => {
                srv_read_err=true;
            },
        }

        //SCRCPY>HU
        match scrcpy_rx.try_recv() {
            Ok(mut msg) => {
                if !ssl_handshake_done
                {
                    error!( "{}: tls proxy error: received encrypted message from service before TLS handshake", get_name());
                }
                else {
                    /*let _ = pkt_debug(
                        HexdumpLevel::DecryptedOutput,
                        dmp_level,
                        &msg,
                        "SCRCPY".parse().unwrap()
                    ).await;*/
                    match msg.encrypt_payload(&mut mem_buf, &mut server).await {
                        Ok(_) => {
                            // Increment byte counters for statistics
                            // fixme: compute final_len for precise stats
                            w_statistics.fetch_add(HEADER_LENGTH + msg.payload.len(), Ordering::Relaxed);
                            msg.transmit(&mut hu_wr).await.with_context(|| format!("{}: transmit to HU failed", get_name()))?;
                        }
                        Err(e) => {error!( "{} encrypt_payload error: {:?}", get_name(), e);},
                    }
                }
            }

            // For both errors (Disconnected and Empty), the correct action
            // is to process the items.  If the error was Disconnected, on
            // the next iteration rx.recv().await will be None and we'll
            // break from the outer loop anyway.
            Err(_) => {
                //if both errors and this one also we must wait to prevent tokio runtime starvation
                if hu_read_err && srv_read_err
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }

            },
        }

    }

    /// checking if there was a true fatal SSL error
    /// Note that the error may not be fatal. For example if the underlying
    /// stream is an asynchronous one then `HandshakeError::WouldBlock` may
    /// just mean to wait for more I/O to happen later.
    fn ssl_check_failure<T>(res: std::result::Result<T, openssl::ssl::Error>) -> Result<()> {
        if let Err(err) = res {
            match err.code() {
                ErrorCode::WANT_READ | ErrorCode::WANT_WRITE | ErrorCode::SYSCALL => Ok(()),
                _ => return Err(Box::new(err)),
            }
        } else {
            Ok(())
        }
    }

    /// creates Ssl for HeadUnit (SSL server) and MobileDevice (SSL client)
    async fn ssl_builder() -> Result<Ssl> {
        let mut ctx_builder = SslContextBuilder::new(SslMethod::tls())?;

        // for HU/headunit we need to act as a MD/mobiledevice, so load "md" key and cert
        // and vice versa
        let prefix = "md";
        ctx_builder.set_certificate_file(format!("{KEYS_PATH}/{prefix}_cert.pem"), SslFiletype::PEM)?;
        ctx_builder.set_private_key_file(format!("{KEYS_PATH}/{prefix}_key.pem"), SslFiletype::PEM)?;
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

    /// encapsulates SSL data into Packet
    async fn ssl_encapsulate(mut mem_buf: SslMemBuf) -> Result<Packet> {
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
}


///Check if recieved pkt.message_id is expected
fn check_control_msg_id(expected: protos::ControlMessageType, pkt: &Packet) -> Result<()> {
    if pkt.channel != 0
    {
        Err(Box::new("Wrong channel number")).expect("Expected 0")
    }
    // message_id is the first 2 bytes of payload
    let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
    if protos::ControlMessageType::from_i32(message_id).unwrap_or(MESSAGE_UNEXPECTED_MESSAGE) != expected {
        Err(Box::new("Wrong message id")).expect("ControlMessageType")
    }
    Ok(())
    /*match protos::ControlMessageType::from_i32(message_id).unwrap_or(MESSAGE_UNEXPECTED_MESSAGE)
    {
        expected => {Ok(())}
        _ => {
            Err(Box::new("Wrong message id")).expect("ControlMessageType")
        }
    }*/
}
///return index from services array
fn get_service_index(arr:&Vec<ServiceStatus>, ch:i32)->usize
{
    for (idx,stat) in arr.iter().enumerate() {
        if stat.ch_id == ch
        {
            return idx;
        }
    }
    255
}

/// main thread doing all packet processing between HU and device
pub async fn ch_proxy(
    mut rx_srv: Receiver<Packet>,
    mut tx_srv: Sender<Packet>,
    scrcpy_cmd_tx: flume::Sender<Packet>,
    scrcpy_cmd_rx: flume::Receiver<Packet>,
) -> Result<()> {
    info!( "{} Entering channel manager",get_name());
   // waiting for initial version frame (HU is starting transmission)
    info!( "{} Waiting for HU version request...",get_name());
    //let pkt = rx_hu.recv().await.ok_or("reader channel hung up")?;
    let pkt = rx_srv.recv().await.ok_or("rx_srv channel hung up")?;
    let chk = check_control_msg_id(MESSAGE_VERSION_REQUEST,&pkt);
    match chk {
        Ok(_v) => info!( "{} HU version request received, sending VersionResponse back...",get_name()),
        Err(e) => {error!( "{} HU sent unexpected channel message", get_name()); return Err(e)},
    }
        // build version response for HU
        //let mut response = VersionResponse::new();
        //let mut payload: Vec<u8> = response.write_to_bytes()?;
        let mut payload: Vec<u8>=Vec::new();
        payload.push(((MESSAGE_VERSION_RESPONSE as u16) >> 8) as u8);
        payload.push( ((MESSAGE_VERSION_RESPONSE as u16) & 0xff) as u8);
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
    if let Err(_) = tx_srv.send(pkt_rsp).await{
        error!( "{} tls proxy send error",get_name());
    };

    info!( "{} Waiting for HU MESSAGE_AUTH_COMPLETE...",get_name());
    let pkt = rx_srv.recv().await.ok_or("rx_srv channel hung up")?;
    let chk = check_control_msg_id(MESSAGE_AUTH_COMPLETE,&pkt);
    match chk {
        Ok(_v) => info!( "{} MESSAGE_AUTH_COMPLETE received",get_name()),
        Err(e) => {error!( "{} HU sent unexpected channel message", get_name()); return Err(e)},
    }
    let data = &pkt.payload[2..]; // start of message data, without message_id
    if let Ok(msg) = AuthResponse::parse_from_bytes(&data) {
        if msg.status() != OK
        {
            error!( "{} AuthResponse status is not OK, got {:?}",get_name(), msg.status);
            return Err(Box::new("AuthResponse status is not OK")).expect("AuthResponse.OK");
        }
    }
    else {
        error!( "{} AuthResponse couldn't be parsed",get_name());
        return Err(Box::new("AuthResponse couldn't be parsed")).expect("AuthResponse");
    }

    info!( "{} Sending ServiceDiscovery request...",get_name());
    let icon32 = std::fs::read(format!("{}{}", RES_PATH, "/AndroidIcon32.png"));
    let icon64 = std::fs::read(format!("{}{}", RES_PATH, "/AndroidIcon64.png"));
    let icon128 = std::fs::read(format!("{}{}", RES_PATH, "/AndroidIcon128.png"));
    let mut sdreq= ServiceDiscoveryRequest::new();
    sdreq.set_small_icon(icon32.unwrap());
    sdreq.set_medium_icon(icon64.unwrap());
    sdreq.set_large_icon(icon128.unwrap());
    sdreq.set_label_text("aa-mirror-rs".to_owned());
    sdreq.set_device_name("aa-mirror-os".to_owned());
    let mut payload: Vec<u8>=sdreq.write_to_bytes()?;
    payload.insert(0,((MESSAGE_SERVICE_DISCOVERY_REQUEST as u16) >> 8) as u8);
    payload.insert( 1,((MESSAGE_SERVICE_DISCOVERY_REQUEST as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: 0,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    if let Err(_) = tx_srv.send(pkt_rsp).await{
        error!( "{} tls proxy send error",get_name());
    };

    info!( "{} Waiting for HU MESSAGE_SERVICE_DISCOVERY_RESPONSE...",get_name());
    let pkt = rx_srv.recv().await.ok_or("rx_srv channel hung up")?;
    let chk = check_control_msg_id(MESSAGE_SERVICE_DISCOVERY_RESPONSE,&pkt);
    match chk {
        Ok(_v) => info!( "{} MESSAGE_SERVICE_DISCOVERY_RESPONSE received",get_name()),
        Err(e) => {error!( "{} HU sent unexpected channel message", get_name()); return Err(e)},
    }
    //let mut srv_senders:Vec<Option<Box<Sender<Packet>>>> = vec![];
    let mut srv_senders;
    let mut srv_tsk_handles;
    let mut channel_status;
    let data = &pkt.payload[2..]; // start of message data, without message_id
    let mut video_codec_params = VideoStreamingParams::default();
    let mut audio_codec_params = AudioStreamingParams::default();
    if  let Ok(msg) = ServiceDiscoveryResponse::parse_from_bytes(&data){
        info!( "{} ServiceDiscoveryResponse parsed ok",get_name());
        //let srv_count=msg.services.len();
        srv_senders=Vec::with_capacity(msg.services.len());
        srv_tsk_handles=Vec::with_capacity(msg.services.len());
        channel_status =Vec::with_capacity(msg.services.len());
        //let mut tsk_srv_loop;
        for (_,proto_srv) in msg.services.iter().enumerate() {
            let ch_id=i32::from(proto_srv.id());
            //let idx=(ch_id-1) as usize;
            //info!( "SID {}, media sink: {}",ch_id, proto_srv.media_sink_service.is_some());

            if proto_srv.media_sink_service.is_some()
            {
                channel_status.push(ServiceStatus{service_type:ServiceType::MediaSink, ch_id, enabled:true, open_ch_cmd: CommandState::NotDone });
                if proto_srv.media_sink_service.audio_configs.len()>0
                {
                    let srv_type=proto_srv.media_sink_service.audio_type();
                    let acd=match proto_srv.media_sink_service.available_type() {
                        MediaCodecType::MEDIA_CODEC_AUDIO_AAC_LC_ADTS=>AUDIO_AAC_LC_ADTS,
                        MediaCodecType::MEDIA_CODEC_AUDIO_AAC_LC=>AUDIO_AAC_LC,
                        MediaCodecType::MEDIA_CODEC_AUDIO_PCM=>AUDIO_PCM,
                        _=>AUDIO_PCM,
                    };
                    if srv_type == AUDIO_STREAM_GUIDANCE
                    {
                        let audio_cfg=AudioConfig
                        {
                            codec:acd,
                            stream_type: AudioStream::GUIDANCE,
                            bitrate:proto_srv.media_sink_service.audio_configs[0].number_of_bits(),
                            channels:proto_srv.media_sink_service.audio_configs[0].number_of_channels(),
                            bitdepth:proto_srv.media_sink_service.audio_configs[0].number_of_bits(),
                        };
                        let (tx, rx):(Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
                        srv_senders.push(tx);
                        srv_tsk_handles.push(tokio_uring::spawn(th_media_sink_audio_guidance(ch_id,true, tx_srv.clone(), rx, audio_cfg)));
                    }
                    else if srv_type == AUDIO_STREAM_MEDIA
                    {
                        let audio_cfg=AudioConfig
                        {
                            codec:acd,
                            stream_type: AudioStream::MEDIA,
                            bitrate:proto_srv.media_sink_service.audio_configs[0].number_of_bits(),
                            channels:proto_srv.media_sink_service.audio_configs[0].number_of_channels(),
                            bitdepth:proto_srv.media_sink_service.audio_configs[0].number_of_bits(),
                        };
                        audio_codec_params.bitrate=audio_cfg.bitrate as i32;
                        audio_codec_params.sid=ch_id;
                        let (tx, rx):(Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
                        srv_senders.push(tx);
                        srv_tsk_handles.push(tokio_uring::spawn(th_media_sink_audio_streaming(ch_id,true, tx_srv.clone(), rx, scrcpy_cmd_tx.clone(), audio_cfg, audio_codec_params.clone())));
                    }
                    else {
                        error!( "{} Service not implemented ATM for ch: {}",get_name(), ch_id);
                    }
                }
                else if proto_srv.media_sink_service.video_configs.len()>0
                {
                    let (tx, rx):(Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
                    srv_senders.push(tx);
                    let vcr=match proto_srv.media_sink_service.video_configs[0].codec_resolution() {
                        VideoCodecResolutionType::VIDEO_800x480=>{ video_codec_params.bitrate =4_000_000; video_codec_params.res_w=800; video_codec_params.res_h=480; Video_800x480},
                        VideoCodecResolutionType::VIDEO_720x1280=>{ video_codec_params.bitrate =8_000_000; video_codec_params.res_w=1280; video_codec_params.res_h=720; Video_720x1280},
                        VideoCodecResolutionType::VIDEO_1080x1920=>{ video_codec_params.bitrate =16_000_000; video_codec_params.res_w=1920; video_codec_params.res_h=1080; Video_1080x1920},
                        _=>{ video_codec_params.bitrate =4_000_000; video_codec_params.res_w=800; video_codec_params.res_h=480; Video_800x480},
                    };

                    let vcd=match proto_srv.media_sink_service.video_configs[0].video_codec_type() {
                        MediaCodecType::MEDIA_CODEC_VIDEO_H264_BP=>VIDEO_H264_BP,
                        MediaCodecType::MEDIA_CODEC_VIDEO_H265=>VIDEO_H265,
                        MediaCodecType::MEDIA_CODEC_AUDIO_PCM=>AUDIO_PCM,
                        _=>VIDEO_H264_BP,
                    };
                    let vfps=match proto_srv.media_sink_service.video_configs[0].frame_rate() {
                        VideoFrameRateType::VIDEO_FPS_60=>{ video_codec_params.fps=60; FPS_60},
                        VideoFrameRateType::VIDEO_FPS_30=>{ video_codec_params.fps=30; FPS_30},
                        _=>{ video_codec_params.fps=30; FPS_30},
                    };
                    video_codec_params.dpi=proto_srv.media_sink_service.video_configs[0].density() as i32;
                    video_codec_params.sid=ch_id;
                    srv_tsk_handles.push(tokio_uring::spawn(th_media_sink_video(ch_id,true, tx_srv.clone(), rx, scrcpy_cmd_tx.clone(), video_codec_params.clone())));
                }
                else {
                    error!( "{} Service not implemented ATM for ch: {}",get_name(), ch_id);
                }
            }
            else if proto_srv.media_source_service.is_some()
            {
                channel_status.push(ServiceStatus{service_type:ServiceType::MediaSource,ch_id,enabled:false, open_ch_cmd: CommandState::NotDone});
                let (tx, rx):(Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
                srv_senders.push(tx);
                srv_tsk_handles.push(tokio_uring::spawn(th_media_source(ch_id,false, tx_srv.clone(), rx)));
            }
            else if proto_srv.sensor_source_service.is_some()
            {
                channel_status.push( ServiceStatus{service_type:ServiceType::SensorSource,ch_id, enabled:false, open_ch_cmd: CommandState::NotDone});
                let (tx, rx):(Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
                srv_senders.push(tx);
                srv_tsk_handles.push(tokio_uring::spawn(th_sensor_source(ch_id,false, tx_srv.clone(), rx)));
            }
            else if proto_srv.input_source_service.is_some()
            {
                channel_status.push(ServiceStatus{service_type:ServiceType::InputSource,ch_id,enabled:false, open_ch_cmd: CommandState::NotDone});
                let (tx, rx):(Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
                srv_senders.push(tx);
                srv_tsk_handles.push(tokio_uring::spawn(th_input_source(ch_id,false, tx_srv.clone(), rx)));
            }
            else if proto_srv.vendor_extension_service.is_some()
            {
                channel_status.push( ServiceStatus{service_type:ServiceType::VendorExtension,ch_id,enabled:false, open_ch_cmd: CommandState::NotDone});
                let (tx, rx):(Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
                srv_senders.push(tx);
                srv_tsk_handles.push(tokio_uring::spawn(th_vendor_extension(ch_id,false, tx_srv.clone(), rx)));
            }
            else if proto_srv.bluetooth_service.is_some()
            {
                channel_status.push(ServiceStatus{service_type:ServiceType::Bluetooth,ch_id,enabled:false, open_ch_cmd: CommandState::NotDone});
                let (tx, rx):(Sender<Packet>, Receiver<Packet>) = mpsc::channel(10);
                srv_senders.push(tx);
                srv_tsk_handles.push(tokio_uring::spawn(th_bluetooth(ch_id, false, tx_srv.clone(), rx)));
            }
            else {
                error!( "{} Service not implemented ATM for ch: {}",get_name(), ch_id);
            }
        }
        info!( "{} Sending AudioFocus request...",get_name());
        let mut focus_req= AudioFocusRequestNotification::new();
        focus_req.set_request(AudioFocusRequestType::AUDIO_FOCUS_GAIN);

        let mut payload: Vec<u8>=focus_req.write_to_bytes()?;
        payload.insert(0,((MESSAGE_AUDIO_FOCUS_REQUEST as u16) >> 8) as u8);
        payload.insert( 1,((MESSAGE_AUDIO_FOCUS_REQUEST as u16) & 0xff) as u8);

        let pkt_rsp = Packet {
            channel: 0,
            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
            final_length: None,
            payload: payload,
        };
        if let Err(_) = tx_srv.send(pkt_rsp).await{
            error!( "{} tls proxy send error",get_name());
        };
    }
    else {
        error!( "{} ServiceDiscoveryResponse couldn't be parsed",get_name());
        return Err(Box::new("ServiceDiscoveryResponse couldn't be parsed")).expect("ServiceDiscoveryResponse");
    }
    info!( "{} ServiceDiscovery done, starting AA Mirror loop",get_name());
    loop {
        let mut pkt = rx_srv.recv().await.ok_or("rx_srv channel hung up")?;
        if pkt.channel !=0
        {
            let ch=pkt.channel;
            let idx=get_service_index(&channel_status, ch as i32);
            if idx !=255
            {
                srv_senders[idx].send(pkt).await.expect("Error sending message to service");
            }
            else {
                error!( "{} Invalid channel {}",get_name(), ch);
            }
        }
        else { //Default channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            let control = protos::ControlMessageType::from_i32(message_id);
            match control.unwrap_or(MESSAGE_UNEXPECTED_MESSAGE) {
                ControlMessageType::MESSAGE_PING_REQUEST =>{
                    let data = &pkt.payload[2..]; // start of message data, without message_id
                    if let Ok(msg) = PingRequest::parse_from_bytes(&data) {
                        let mut pingrsp= PingResponse::new();
                        pingrsp.set_timestamp(msg.timestamp());
                        let mut payload: Vec<u8>=pingrsp.write_to_bytes()?;
                        payload.insert(0,((MESSAGE_PING_RESPONSE as u16) >> 8) as u8);
                        payload.insert( 1,((MESSAGE_PING_RESPONSE as u16) & 0xff) as u8);
                        let pkt_rsp = Packet {
                            channel: 0,
                            flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                            final_length: None,
                            payload: payload,
                        };
                        if let Err(_) = tx_srv.send(pkt_rsp).await{
                            error!( "{} tls proxy send error",get_name());
                        };
                    }
                    else {
                        error!( "{} PingRequest couldn't be parsed",get_name());
                    }

                }
                ControlMessageType::MESSAGE_AUDIO_FOCUS_NOTIFICATION =>{
                    let data = &pkt.payload[2..]; // start of message data, without message_id
                    if let Ok(msg) = AudioFocusNotification::parse_from_bytes(&data) {
                        info!( "{} AUDIO_FOCUS_STATE received is: {:?}",get_name(), msg.focus_state());
                        if msg.focus_state() == AudioFocusStateType::AUDIO_FOCUS_STATE_GAIN
                        {
                            info!( "{} CMD OPEN_CHANNEL will be done next",get_name());
                            tokio::time::sleep(Duration::from_millis(HU_CONFIG_DELAY_MS)).await; //reconfiguration time for HU
                            //Open CH for all
                            for (idx, _) in srv_senders.iter().enumerate()
                            {
                                info!( "{} Send custom CMD_OPEN_CH for ch {}",get_name(), channel_status[idx].ch_id);

                                let mut payload= Vec::new();
                                payload.extend_from_slice(&(MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                                payload.extend_from_slice(&(CustomCommand::CMD_OPEN_CH as u16).to_be_bytes());
                                let pkt_rsp = Packet {
                                    channel: (channel_status[idx].ch_id) as u8,
                                    flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                    final_length: None,
                                    payload: payload.clone(),
                                };
                                channel_status[idx].open_ch_cmd = CommandState::InProgress;
                                if let Err(_) = srv_senders[idx].send(pkt_rsp).await{
                                    error!( "{} custom command send error",get_name());
                                };
                            }
                        }

                    }
                    else {
                        error!( "{} AudioFocusNotification couldn't be parsed",get_name());
                    }

                }
                _ =>{ info!( "{} Unknown message ID: {} received for default channel",get_name(), message_id);}
            };
        }

        //check for MD connected
        match scrcpy_cmd_rx.try_recv() {
            Ok(mut pkt) => {
                let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
                if message_id == MESSAGE_CUSTOM_CMD  as i32
                {
                    let cmd_id: i32 = u16::from_be_bytes(pkt.payload[2..=3].try_into()?).into();
                    if cmd_id == CustomCommand::MD_CONNECTED as i32
                    {
                        let ch=pkt.channel as i32;
                        info!("{} MD connected, proxy packet to media channels",get_name());
                        let idx=get_service_index(&channel_status, audio_codec_params.sid);
                        if idx !=255
                        {
                            let mut payload: Vec<u8>=Vec::new();
                            payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                            payload.extend_from_slice(&(CustomCommand::MD_CONNECTED as u16).to_be_bytes());
                            let pkt_rsp = Packet {
                                channel: audio_codec_params.sid as u8,
                                flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                final_length: None,
                                payload: std::mem::take(&mut payload),
                            };
                            srv_senders[idx].send(pkt_rsp).await.expect("Error sending message to service");
                        }
                        else {
                            error!( "{} Invalid channel {}",get_name(), ch);
                        }
                        let idx=get_service_index(&channel_status, video_codec_params.sid);
                        if idx !=255
                        {
                            let mut payload: Vec<u8>=Vec::new();
                            payload.extend_from_slice(&(ControlMessageType::MESSAGE_CUSTOM_CMD as u16).to_be_bytes());
                            payload.extend_from_slice(&(CustomCommand::MD_CONNECTED as u16).to_be_bytes());
                            let pkt_rsp = Packet {
                                channel: video_codec_params.sid as u8,
                                flags: FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                final_length: None,
                                payload: std::mem::take(&mut payload),
                            };
                            srv_senders[idx].send(pkt_rsp).await.expect("Error sending message to service");
                        }
                        else {
                            error!( "{} Invalid channel {}",get_name(), ch);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    return Err(Box::new("proxy main loop ended ok")).expect("TODO");
}
