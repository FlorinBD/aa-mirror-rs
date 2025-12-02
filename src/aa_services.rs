use log::log_enabled;
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

trait IService {
    fn handle_hu_msg(&self, a: &str);
}
