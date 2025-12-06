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
use crate::aa_services::MediaMessageId::*;
use crate::aa_services::SensorMessageId::*;
use crate::aa_services::SensorType::*;
use protobuf::text_format::print_to_string_pretty;
use protobuf::{Enum, EnumOrUnknown, Message, MessageDyn};
use protos::ControlMessageType::{self, *};
use crate::aoa::AccessoryDeviceInfo;
use crate::channel_manager::Packet;

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

impl fmt::Display for protos::MediaMessageId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            protos::MediaMessageId::MEDIA_MESSAGE_DATA => write!(f, "MEDIA_MESSAGE_DATA"),
            protos::MediaMessageId::MEDIA_MESSAGE_CODEC_CONFIG => write!(f, "MEDIA_MESSAGE_CODEC_CONFIG"),
            protos::MediaMessageId::MEDIA_MESSAGE_SETUP => write!(f, "MEDIA_MESSAGE_SETUP"),
            protos::MediaMessageId::MEDIA_MESSAGE_START => write!(f, "MEDIA_MESSAGE_START"),
            protos::MediaMessageId::MEDIA_MESSAGE_STOP => write!(f, "MEDIA_MESSAGE_STOP"),
            protos::MediaMessageId::MEDIA_MESSAGE_CONFIG => write!(f, "MEDIA_MESSAGE_CONFIG"),
            protos::MediaMessageId::MEDIA_MESSAGE_ACK => write!(f, "MEDIA_MESSAGE_ACK"),
            protos::MediaMessageId::MEDIA_MESSAGE_MICROPHONE_REQUEST => write!(f, "MEDIA_MESSAGE_MICROPHONE_REQUEST"),
            protos::MediaMessageId::MEDIA_MESSAGE_MICROPHONE_RESPONSE => write!(f, "MEDIA_MESSAGE_MICROPHONE_RESPONSE"),
            protos::MediaMessageId::MEDIA_MESSAGE_VIDEO_FOCUS_REQUEST => write!(f, "MEDIA_MESSAGE_VIDEO_FOCUS_REQUEST"),
            protos::MediaMessageId::MEDIA_MESSAGE_VIDEO_FOCUS_NOTIFICATION => write!(f, "MEDIA_MESSAGE_VIDEO_FOCUS_NOTIFICATION"),
            protos::MediaMessageId::MEDIA_MESSAGE_UPDATE_UI_CONFIG_REQUEST => write!(f, "MEDIA_MESSAGE_UPDATE_UI_CONFIG_REQUEST"),
            protos::MediaMessageId::MEDIA_MESSAGE_UPDATE_UI_CONFIG_REPLY => write!(f, "MEDIA_MESSAGE_UPDATE_UI_CONFIG_REPLY"),
            protos::MediaMessageId::MEDIA_MESSAGE_AUDIO_UNDERFLOW_NOTIFICATION => write!(f, "MEDIA_MESSAGE_AUDIO_UNDERFLOW_NOTIFICATION"),
            _ => write!(f, "Unknown_MediaMessageId"),
        }
    }
}
pub trait IService{
    fn handle_hu_msg(&self, pkt: &Packet)->();
    fn get_service_type(&self)->ServiceType;
}

pub struct MediaSinkService {
    sid: ServiceType,
}

impl Clone for MediaSinkService {
    fn clone(&self) -> Self {
        MediaSinkService {
            sid: self.sid.clone()
        }
    }
}

impl MediaSinkService {
    pub fn new() -> Self {
        Self{
            sid:ServiceType::MediaSink,
        }
    }
}

impl IService for MediaSinkService {
    fn handle_hu_msg(&self, pkt: &Packet)
    {
        if let Ok(id)=pkt.payload[0..=1].try_into()
        {
            let message_id: i32 = u16::from_be_bytes(id).into();

            let control = protos::MediaMessageId::from_i32(message_id);
            match control.unwrap() {
                MEDIA_MESSAGE_VIDEO_FOCUS_NOTIFICATION => {
                    info!("{} Received {} message", self.sid.to_string(), control.to_string());
                }
                _ =>{ error!( "{} Unhandled message ID: {} received",self.sid.to_string(), control.to_string());}
            }
        }

    }

    fn get_service_type(&self)->ServiceType
    {
        return self.sid;
    }
}

pub struct MediaSourceService {
    pub(crate) sid: ServiceType,
}

impl IService for MediaSourceService {
    fn handle_hu_msg(&self, pkt: &Packet)
    {

    }

    fn get_service_type(&self)->ServiceType
    {
        return self.sid;
    }
}
