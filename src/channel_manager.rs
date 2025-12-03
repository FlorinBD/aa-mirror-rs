use anyhow::Context;
use log::log_enabled;
use openssl::ssl::{ErrorCode, Ssl, SslContextBuilder, SslFiletype, SslMethod};
use simplelog::*;
use std::collections::VecDeque;
use std::fmt;
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
use crate::channel_manager::protos::navigation_maneuver::NavigationType::*;
use crate::channel_manager::protos::auth_response::Status::*;
use crate::channel_manager::protos::Config as AudioConfig;
use crate::channel_manager::protos::*;
use crate::channel_manager::sensor_source_service::Sensor;
use crate::channel_manager::AudioStreamType::*;
use crate::channel_manager::ByeByeReason::USER_SELECTION;
use crate::channel_manager::MediaMessageId::*;
use crate::channel_manager::SensorMessageId::*;
use crate::channel_manager::SensorType::*;
use protobuf::text_format::print_to_string_pretty;
use protobuf::{Enum, EnumOrUnknown, Message, MessageDyn};
use protos::ControlMessageType::{self, *};

use crate::aa_services::{
    MediaSinkService,MediaSourceService,ServiceType,IService,
};
use crate::config::{Action::Stop, AppConfig, SharedConfig};
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
const _CONTROL: u8 = 1 << 2;
pub const ENCRYPTED: u8 = 1 << 3;

// location for hu_/md_ private keys and certificates:
const KEYS_PATH: &str = "/etc/aa-mirror-rs";
const RES_PATH: &str = "/etc/aa-mirror-rs/res";

pub struct ModifyContext {
    sensor_channel: Option<u8>,
    nav_channel: Option<u8>,
    audio_channels: Vec<u8>,
}

#[derive(PartialEq, Copy, Clone, Debug)]
pub enum DeviceType {
    HeadUnit,
    MobileDevice,
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
    debug!("message_id = {:04X}, {:?}", message_id, control);
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
        _ => return Ok(()),
    };
    // show pretty string from the message
    debug!("{}", print_to_string_pretty(message));

    Ok(())
}

/// packet modification hook
pub async fn pkt_modify_hook(
    pkt: &mut Packet,
    ctx: &mut ModifyContext,
    sensor_channel: Arc<tokio::sync::Mutex<Option<u8>>>,
    cfg: &AppConfig,
    config: &mut SharedConfig,
) -> Result<bool> {
    // if for some reason we have too small packet, bail out
    if pkt.payload.len() < 2 {
        return Ok(false);
    }

    // message_id is the first 2 bytes of payload
    let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
    let data = &pkt.payload[2..]; // start of message data

    // handling data on sensor channel
    if let Some(ch) = ctx.sensor_channel {
        if ch == pkt.channel {
            match protos::SensorMessageId::from_i32(message_id).unwrap_or(SENSOR_MESSAGE_ERROR) {
                SENSOR_MESSAGE_REQUEST => {
                    if let Ok(msg) = SensorRequest::parse_from_bytes(data) {
                        if msg.type_() == SensorType::SENSOR_VEHICLE_ENERGY_MODEL_DATA {
                            debug!(
                                "additional SENSOR_MESSAGE_REQUEST for {:?}, making a response with success...",
                                msg.type_()
                            );
                            let mut response = SensorResponse::new();
                            response.set_status(MessageStatus::STATUS_SUCCESS);

                            let mut payload: Vec<u8> = response.write_to_bytes()?;
                            payload.insert(0, ((SENSOR_MESSAGE_RESPONSE as u16) >> 8) as u8);
                            payload.insert(1, ((SENSOR_MESSAGE_RESPONSE as u16) & 0xff) as u8);

                            let reply = Packet {
                                channel: ch,
                                flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
                                final_length: None,
                                payload: payload,
                            };
                            *pkt = reply;

                            // return true => send own reply without processing
                            return Ok(true);
                        }
                    }
                }
                SENSOR_MESSAGE_BATCH => {
                    if let Ok(mut msg) = SensorBatch::parse_from_bytes(data) {
                        if cfg.video_in_motion {
                            if !msg.driving_status_data.is_empty() {
                                // forcing status to 0 value
                                msg.driving_status_data[0].set_status(0);
                                // regenerating payload data
                                pkt.payload = msg.write_to_bytes()?;
                                pkt.payload.insert(0, (message_id >> 8) as u8);
                                pkt.payload.insert(1, (message_id & 0xff) as u8);
                            }
                        }
                    }
                }
                _ => (),
            }
            // end sensors processing
            return Ok(false);
        }
    }

    // apply waze workaround on navigation data
    if let Some(ch) = ctx.nav_channel {
        // check for channel and a specific packet header only
        if ch == pkt.channel
            && pkt.payload[0] == 0x80
            && pkt.payload[1] == 0x06
            && pkt.payload[2] == 0x0A
        {
            if let Ok(mut msg) = NavigationState::parse_from_bytes(&data) {
                if msg.steps[0].maneuver.type_() == U_TURN_LEFT {
                    msg.steps[0]
                        .maneuver
                        .as_mut()
                        .unwrap()
                        .set_type(U_TURN_RIGHT);
                    info!(
                        "{} swapped U_TURN_LEFT to U_TURN_RIGHT",
                        get_name()
                    );

                    // rewrite payload to new message contents
                    pkt.payload = msg.write_to_bytes()?;
                    // inserting 2 bytes of message_id at the beginning
                    pkt.payload.insert(0, (message_id >> 8) as u8);
                    pkt.payload.insert(1, (message_id & 0xff) as u8);
                    return Ok(false);
                }
            }
            // end navigation service processing
            return Ok(false);
        }
    }

    // if configured, override max_unacked for matching audio channels
    if cfg.audio_max_unacked > 0
        && ctx.audio_channels.contains(&pkt.channel)
    {
        match protos::MediaMessageId::from_i32(message_id).unwrap_or(MEDIA_MESSAGE_DATA) {
            m @ MEDIA_MESSAGE_CONFIG => {
                if let Ok(mut msg) = AudioConfig::parse_from_bytes(&data) {
                    // get previous/original value
                    let prev_val = msg.max_unacked();
                    // set new value
                    msg.set_max_unacked(cfg.audio_max_unacked.into());

                    info!(
                        "{} <yellow>{:?}</>: overriding max audio unacked from <b>{}</> to <b>{}</> for channel: <b>{:#04x}</>",
                        get_name(),
                        m,
                        prev_val,
                        cfg.audio_max_unacked,
                        pkt.channel,
                    );

                    // FIXME: this code fragment is used multiple times
                    // rewrite payload to new message contents
                    pkt.payload = msg.write_to_bytes()?;
                    // inserting 2 bytes of message_id at the beginning
                    pkt.payload.insert(0, (message_id >> 8) as u8);
                    pkt.payload.insert(1, (message_id & 0xff) as u8);
                    return Ok(false);
                }
                // end processing
                return Ok(false);
            }
            _ => (),
        }
    }

    if pkt.channel != 0 {
        return Ok(false);
    }
    // trying to obtain an Enum from message_id
    let control = protos::ControlMessageType::from_i32(message_id);
    debug!("message_id = {:04X}, {:?}", message_id, control);

    // parsing data
    match control.unwrap_or(MESSAGE_UNEXPECTED_MESSAGE) {
        MESSAGE_BYEBYE_REQUEST => {
            if cfg.stop_on_disconnect {
                if let Ok(msg) = ByeByeRequest::parse_from_bytes(data) {
                    if msg.reason.unwrap_or_default() == USER_SELECTION.into() {
                        info!(
                        "{} <bold><blue>Disconnect</> option selected in Android Auto; auto-connect temporarily disabled",
                        get_name(),
                    );
                        config.write().await.action_requested = Some(Stop);
                    }
                }
            }
        }
        MESSAGE_SERVICE_DISCOVERY_RESPONSE => {
            // rewrite HeadUnit message only, exit if it is MobileDevice
           
            let mut msg = match ServiceDiscoveryResponse::parse_from_bytes(data) {
                Err(e) => {
                    error!(
                        "{} error parsing SDR: {}, ignored!",
                        get_name(),
                        e
                    );
                    return Ok(false);
                }
                Ok(msg) => msg,
            };

            // DPI
            if cfg.dpi > 0 {
                if let Some(svc) = msg
                    .services
                    .iter_mut()
                    .find(|svc| !svc.media_sink_service.video_configs.is_empty())
                {
                    // get previous/original value
                    let prev_val = svc.media_sink_service.video_configs[0].density();
                    // set new value
                    svc.media_sink_service.as_mut().unwrap().video_configs[0]
                        .set_density(cfg.dpi.into());
                    info!(
                        "{} <yellow>{:?}</>: replacing DPI value: from <b>{}</> to <b>{}</>",
                        get_name(),
                        control.unwrap(),
                        prev_val,
                        cfg.dpi
                    );
                }
            }

            // disable tts sink
            if cfg.disable_tts_sink {
                while let Some(svc) = msg.services.iter_mut().find(|svc| {
                    !svc.media_sink_service.audio_configs.is_empty()
                        && svc.media_sink_service.audio_type() == AUDIO_STREAM_GUIDANCE
                }) {
                    svc.media_sink_service
                        .as_mut()
                        .unwrap()
                        .set_audio_type(AUDIO_STREAM_SYSTEM_AUDIO);
                }
                info!(
                    "{} <yellow>{:?}</>: TTS sink disabled",
                    get_name(),
                    control.unwrap(),
                );
            }

            // disable media sink
            if cfg.disable_media_sink {
                msg.services
                    .retain(|svc| svc.media_sink_service.audio_type() != AUDIO_STREAM_MEDIA);
                info!(
                    "{} <yellow>{:?}</>: media sink disabled",
                    get_name(),
                    control.unwrap(),
                );
            }

            // save all audio sink channels in context
            if cfg.audio_max_unacked > 0 {
                for svc in msg
                    .services
                    .iter()
                    .filter(|svc| !svc.media_sink_service.audio_configs.is_empty())
                {
                    ctx.audio_channels.push(svc.id() as u8);
                }
                info!(
                    "{} <blue>media_sink_service:</> channels: <b>{:02x?}</>",
                    get_name(),
                    ctx.audio_channels
                );
            }

            // save sensor channel in context
            if cfg.ev || cfg.video_in_motion {
                if let Some(svc) = msg
                    .services
                    .iter()
                    .find(|svc| !svc.sensor_source_service.sensors.is_empty())
                {
                    // set in local context
                    ctx.sensor_channel = Some(svc.id() as u8);
                    // set in REST server context for remote EV requests
                    let mut sc_lock = sensor_channel.lock().await;
                    *sc_lock = Some(svc.id() as u8);

                    info!(
                        "{} <blue>sensor_source_service</> channel is: <b>{:#04x}</>",
                        get_name(),
                        svc.id() as u8
                    );
                }
            }

            // save navigation channel in context
            if cfg.waze_lht_workaround {
                if let Some(svc) = msg
                    .services
                    .iter()
                    .find(|svc| svc.navigation_status_service.is_some())
                {
                    // set in local context
                    ctx.nav_channel = Some(svc.id() as u8);

                    info!(
                        "{} <blue>navigation_status_service</> channel is: <b>{:#04x}</>",
                        get_name(),
                        svc.id() as u8
                    );
                }
            }

            // remove tap restriction by removing SENSOR_SPEED
            if cfg.remove_tap_restriction {
                if let Some(svc) = msg
                    .services
                    .iter_mut()
                    .find(|svc| !svc.sensor_source_service.sensors.is_empty())
                {
                    svc.sensor_source_service
                        .as_mut()
                        .unwrap()
                        .sensors
                        .retain(|s| s.sensor_type() != SENSOR_SPEED);
                }
            }

            // enabling developer mode
            if cfg.developer_mode {
                msg.set_make("Google".into());
                msg.set_model("Desktop Head Unit".into());
                info!(
                    "{} <yellow>{:?}</>: enabling developer mode",
                    get_name(),
                    control.unwrap(),
                );
            }

            if cfg.remove_bluetooth {
                msg.services.retain(|svc| svc.bluetooth_service.is_none());
            }

            if cfg.remove_wifi {
                msg.services
                    .retain(|svc| svc.wifi_projection_service.is_none());
            }

            // EV routing features
            if cfg.ev {
                if let Some(svc) = msg
                    .services
                    .iter_mut()
                    .find(|svc| !svc.sensor_source_service.sensors.is_empty())
                {
                    info!(
                        "{} <yellow>{:?}</>: adding <b><green>EV</> features...",
                        get_name(),
                        control.unwrap(),
                    );

                    // add VEHICLE_ENERGY_MODEL_DATA sensor
                    let mut sensor = Sensor::new();
                    sensor.set_sensor_type(SENSOR_VEHICLE_ENERGY_MODEL_DATA);
                    svc.sensor_source_service
                        .as_mut()
                        .unwrap()
                        .sensors
                        .push(sensor);

                    // set FUEL_TYPE
                    svc.sensor_source_service
                        .as_mut()
                        .unwrap()
                        .supported_fuel_types = vec![FuelType::FUEL_TYPE_ELECTRIC.into()];

                    // supported connector types
                    let connectors: Vec<EnumOrUnknown<EvConnectorType>> =
                        match &cfg.ev_connector_types.0 {
                            Some(types) => types.iter().map(|&t| t.into()).collect(),
                            None => {
                                vec![EvConnectorType::EV_CONNECTOR_TYPE_MENNEKES.into()]
                            }
                        };
                    info!(
                        "{} <yellow>{:?}</>: EV connectors: {:?}",
                        get_name(),
                        control.unwrap(),
                        connectors,
                    );
                    svc.sensor_source_service
                        .as_mut()
                        .unwrap()
                        .supported_ev_connector_types = connectors;
                }
            }

            debug!(
                "{} SDR after changes: {}",
                get_name(),
                protobuf::text_format::print_to_string_pretty(&msg)
            );

            // rewrite payload to new message contents
            pkt.payload = msg.write_to_bytes()?;
            // inserting 2 bytes of message_id at the beginning
            pkt.payload.insert(0, (message_id >> 8) as u8);
            pkt.payload.insert(1, (message_id & 0xff) as u8);
        }
        _ => return Ok(false),
    };

    Ok(false)
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
                    info!("Channel {} recieved {} bytes from HU", channel ,payload_size);
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
///Send a message to HU
async fn hu_send_msg<A: Endpoint<A>>(device: &mut IoDevice<A>, flags: u8, payload: Vec<u8>, ssl_buf: &mut SslMemBuf, ssl_stream: &mut openssl::ssl::SslStream<SslMemBuf>, statistics: Arc<AtomicUsize>, dmp_level:HexdumpLevel) -> Result<()> {
    let mut pkt_rsp = Packet {
        channel: 0,
        flags: flags,
        final_length: None,
        payload: payload,
    };
    if flags & ENCRYPTED !=0
    {
        pkt_rsp.encrypt_payload(ssl_buf, ssl_stream).await?;
    }
    else {

    }
    // sending reply back to the HU
    let _ = pkt_debug(HexdumpLevel::RawOutput, dmp_level, &pkt_rsp).await;
    pkt_rsp.transmit(device).await.with_context(|| format!("{}: transmit failed", get_name()))?;
    // Increment byte counters for statistics
    // fixme: compute final_len for precise stats
    statistics.fetch_add(HEADER_LENGTH + pkt_rsp.payload.len(), Ordering::Relaxed);
    Ok(())
}

/// main thread doing all packet processing of an endpoint/device
pub async fn proxy<A: Endpoint<A> + 'static>(
    mut device: IoDevice<A>,
    bytes_written: Arc<AtomicUsize>,
    mut rxr: Receiver<Packet>,
    mut config: SharedConfig,
    sensor_channel: Arc<tokio::sync::Mutex<Option<u8>>>,
) -> Result<()> {
    info!( "{} Entering channel manager",get_name());
    let cfg = config.read().await.clone();
    let hex_requested = cfg.hexdump_level;

    let ssl = ssl_builder().await?;

    let mut mem_buf = SslMemBuf {
        client_stream: Arc::new(Mutex::new(VecDeque::new())),
        server_stream: Arc::new(Mutex::new(VecDeque::new())),
    };
    let mut server = openssl::ssl::SslStream::new(ssl, mem_buf.clone())?;

    // initial phase: passing version and doing SSL handshake
   // waiting for initial version frame (HU is starting transmission)
    info!( "{} Waiting for HU version request...",get_name());
    let pkt = rxr.recv().await.ok_or("reader channel hung up")?;
    let _ = pkt_debug(
        HexdumpLevel::DecryptedInput, // the packet is not encrypted
        hex_requested,
        &pkt,
    ).await;
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
        let wr = hu_send_msg(&mut device,FRAME_TYPE_FIRST | FRAME_TYPE_LAST, payload, &mut mem_buf, &mut server, bytes_written.clone(),hex_requested).await;
        match wr {
            Ok(..)=>(),
            Err(e) => {error!( "{} Error sending message to HU", get_name()); return Err(e)},
        }
        // doing SSL handshake
        const STEPS: u8 = 2;
        for i in 1..=STEPS {
            let pkt = rxr.recv().await.ok_or("reader channel hung up")?;
            let _ = pkt_debug(HexdumpLevel::RawInput, hex_requested, &pkt).await;
            pkt.ssl_decapsulate_write(&mut mem_buf).await?;
            ssl_check_failure(server.accept())?;
            info!(
                "{} 🔒 stage #{} of {}: SSL handshake: {}",
                get_name(),
                i,
                STEPS,
                server.ssl().state_string_long(),
            );
            if server.ssl().is_init_finished() {
                info!(
                    "{} 🔒 SSL init complete, negotiated cipher: <b><blue>{}</>",
                    get_name(),
                    server.ssl().current_cipher().unwrap().name(),
                );
            }
            let pkt = ssl_encapsulate(mem_buf.clone()).await?;
            let _ = pkt_debug(HexdumpLevel::RawOutput, hex_requested, &pkt).await;
            pkt.transmit(&mut device)
                .await
                .with_context(|| format!("{}: transmit failed", get_name()))?;
        }
    info!( "{} Waiting for HU MESSAGE_AUTH_COMPLETE...",get_name());
    let pkt = rxr.recv().await.ok_or("reader channel hung up")?;
    let _ = pkt_debug(
        HexdumpLevel::RawInput,
        hex_requested,
        &pkt,
    ).await;
    let chk = check_control_msg_id(MESSAGE_AUTH_COMPLETE,&pkt);
    match chk {
        Ok(v) => info!( "{} MESSAGE_AUTH_COMPLETE received",get_name()),
        Err(e) => {error!( "{} HU sent unexpected channel message", get_name()); return Err(e)},
    }
    let data = &pkt.payload[2..]; // start of message data, without message_id
    if let Ok(msg) = AuthResponse::parse_from_bytes(&data) {
        if msg.status() !=  OK
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
    let wr=hu_send_msg(&mut device,ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST, payload, &mut mem_buf, &mut server, bytes_written.clone(),hex_requested).await;
    match wr {
        Ok(..)=>(),
        Err(e) => {error!( "{} Error sending message to HU", get_name()); return Err(e)},
    }
    info!( "{} Waiting for HU MESSAGE_SERVICE_DISCOVERY_RESPONSE...",get_name());
    let mut pkt = rxr.recv().await.ok_or("reader channel hung up")?;
    match pkt.decrypt_payload(&mut mem_buf, &mut server).await {
        Ok(_) => {
            let _ = pkt_debug(
                HexdumpLevel::DecryptedInput,
                hex_requested,
                &pkt,
            ).await;

        }
        Err(e) => {error!( "{} decrypt_payload error: {:?}", get_name(), e); return Err(e)},
    }
    let chk = check_control_msg_id(MESSAGE_SERVICE_DISCOVERY_RESPONSE,&pkt);
    match chk {
        Ok(v) => info!( "{} MESSAGE_SERVICE_DISCOVERY_RESPONSE received",get_name()),
        Err(e) => {error!( "{} HU sent unexpected channel message", get_name()); return Err(e)},
    }
    let mut aa_sids:Vec<Box<dyn IService>> = Vec::new();
    let data = &pkt.payload[2..]; // start of message data, without message_id
    if let Ok(msg) = ServiceDiscoveryResponse::parse_from_bytes(&data) {
        msg.services.len();
        for (idx,srv) in msg.services.iter().enumerate() {
            aa_sids.insert(idx,srv.id);
        }
    }
    else {
        error!( "{} ServiceDiscoveryResponse couldn't be parsed",get_name());
        return Err(Box::new("ServiceDiscoveryResponse couldn't be parsed")).expect("ServiceDiscoveryResponse");
    }
    info!( "{} ServiceDiscoveryResponse , starting AA Mirror loop",get_name());
    loop {
        let mut pkt = rxr.recv().await.ok_or("reader channel hung up")?;
        match pkt.decrypt_payload(&mut mem_buf, &mut server).await {
            Ok(_) => {
                let _ = pkt_debug(
                    HexdumpLevel::DecryptedInput,
                    hex_requested,
                    &pkt,
                ).await;

            }
            Err(e) => {error!( "{} decrypt_payload error: {:?}", get_name(), e); return Err(e)},
        }
        if pkt.channel !=0
        {

        }
        else {
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            match message_id {
                MESSAGE_PING_REQUEST=>{

                }
                _ =>{ info!( "{} Unknown message ID: {} received for default channel",get_name(), message_id);}
            }
        }

    }
    return Err(Box::new("proxy main loop ended ok")).expect("TODO");
}
