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

pub enum ServiceType
{
    InputSource,
    MediaSink,
    MediaSource,
    SensorSource,
    VendorExtension,
}
pub trait IService {
    fn new(tp:ServiceType) -> Self;
    fn handle_hu_msg(&self, a: &str);
    fn get_channel_id(&self)->u8;
    fn get_service_type(&self)->ServiceType;
}

pub struct ServiceProps {
    pub(crate) channel_id: u8,
    pub(crate) sid: ServiceType,
}

pub struct MediaSinkService {
    id: ServiceProps,
}

impl IService for MediaSinkService {
    fn new(tp: ServiceType) -> Self {
        Self.id.sid=tp;
        !()
    }

    fn handle_hu_msg(&self, a: &str)
    {

    }
    fn get_channel_id(&self)->u8
    {
        return self.id.channel_id;
    }

    fn get_service_type(&self)->ServiceType
    {
        return self.id.sid;
    }
}

pub struct MediaSourceService {
    pub(crate) id: ServiceProps,
}

impl IService for MediaSourceService {
    fn new(tp: ServiceType) -> Self {
        Self.id.sid=tp;
        !()
    }
    fn handle_hu_msg(&self, a: &str)
    {

    }
    fn get_channel_id(&self)->u8
    {
        return self.id.channel_id;
    }
    fn get_service_type(&self)->ServiceType
    {
        return Self.id.sid;
    }
}
