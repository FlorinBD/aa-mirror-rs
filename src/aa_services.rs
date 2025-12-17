//! This crate provides service implementation for  [Android Open Accessory Protocol 1.0](https://source.android.com/devices/accessories/aoa)
use log::log_enabled;
use simplelog::*;
use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use nusb::DeviceInfo;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::timeout;
use tokio_uring::buf::BoundedBuf;

// protobuf stuff:
include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
use crate::aa_services::protos::navigation_maneuver::NavigationType::*;
use crate::aa_services::protos::auth_response::Status::*;
use crate::aa_services::protos::Config as AudioConfig;
use crate::aa_services::protos::*;
use crate::aa_services::sensor_source_service::Sensor;
use crate::aa_services::AudioStreamType::*;
use crate::aa_services::ByeByeReason::USER_SELECTION;
use crate::aa_services::MessageStatus::*;
use crate::aa_services::MediaMessageId::*;
use crate::aa_services::InputMessageId::*;
use crate::aa_services::SensorMessageId::*;
use crate::aa_services::SensorType::*;
use protobuf::text_format::print_to_string_pretty;
use protobuf::{Enum, EnumOrUnknown, Message, MessageDyn};
use protos::ControlMessageType::{self, *};
use crate::aoa::AccessoryDeviceInfo;
use crate::channel_manager::{Packet, ENCRYPTED, FRAME_TYPE_FIRST, FRAME_TYPE_LAST};
use crate::io_uring::Endpoint;
use crate::io_uring::IoDevice;

#[derive(Copy, Clone, Debug)]
pub enum ServiceType
{
    InputSource,
    MediaSink,
    MediaSource,
    SensorSource,
    VendorExtension,
}
impl fmt::Display for ServiceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
        // or, alternatively:
        // fmt::Debug::fmt(self, f)
    }
}

pub async fn th_media_sink(ch_id: i32, tx_srv: Sender<Packet>, mut rx_srv: Receiver<Packet>)-> Result<()>{
    info!( "{}: Starting...", get_name());
    let mut sdreq= ChannelOpenRequest::new();
    sdreq.set_priority(0);
    sdreq.set_service_id(ch_id);
    let mut payload: Vec<u8>=sdreq.write_to_bytes().expect("serialization failed");
    payload.insert(0,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) >> 8) as u8);
    payload.insert( 1,((MESSAGE_CHANNEL_OPEN_REQUEST as u16) & 0xff) as u8);

    let pkt_rsp = Packet {
        channel: 0,
        flags: ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST,
        final_length: None,
        payload: payload,
    };
    tx_srv.send(pkt_rsp).await.expect("TODO: panic message")?;
    loop {
        let mut pkt = rx_srv.recv().await.ok_or("rx_srv channel hung up")?;
        if pkt.channel !=ch_id as u8
        {
            error!( "{} Channel id {:?} is wrong, message discarded",get_name(), pkt.channel);
        }
        else { //Channel messages
            let message_id: i32 = u16::from_be_bytes(pkt.payload[0..=1].try_into()?).into();
            let control = protos::MediaMessageId::from_i32(message_id);
            match control.unwrap_or(MESSAGE_UNEXPECTED_MESSAGE) {
                MEDIA_MESSAGE_CHANNEL_OPEN_RESPONSE =>{
                    info!("{} Received {} message", ch_id.to_string(), message_id);
                    let data = &pkt.payload[2..]; // start of message data, without message_id
                    if let Ok(msg) = ChannelOpenResponse::parse_from_bytes(&data) {
                        if msg.status() != STATUS_SUCCESS
                        {
                            error!( "{}: ChannelOpenResponse status is not OK, got {:?}",get_name(), msg.status);
                        }
                        else {
                            info!( "{}: ChannelOpenResponse received, waiting for media focus",get_name());
                        }
                    }
                    else {
                        error!( "{}: ChannelOpenResponse couldn't be parsed",get_name());
                    }

                }
                _ =>{ info!( "{} Unknown message ID: {} received",get_name(), message_id);}
            };
        }
    }

    fn get_name() -> String {
        let dev = "MediaSinkService";
        format!("<i><bright-black> aa-mirror/{}: </>", dev)
    }
}