//! A logging framework for eBPF programs.
//!
//! This is the user space side of the [Aya] logging framework. For the eBPF
//! side, see the `aya-log-ebpf` crate.
//!
//! `aya-log` provides the [BpfLogger] type, which reads log records created by
//! `aya-log-ebpf` and logs them using the [log] crate. Any logger that
//! implements the [Log] trait can be used with this crate.
//!
//! # Example:
//!
//! This example uses the [env_logger] crate to log messages to the terminal.
//!
//! ```no_run
//! # let mut bpf = aya::Bpf::load(&[]).unwrap();
//! use aya_log::BpfLogger;
//!
//! // initialize env_logger as the default logger
//! env_logger::init();
//!
//! // start reading aya-log records and log them using the default logger
//! BpfLogger::init(&mut bpf).unwrap();
//! ```
//!
//! With the following eBPF code:
//!
//! ```ignore
//! # let ctx = ();
//! use aya_log_ebpf::{debug, error, info, trace, warn};
//!
//! error!(&ctx, "this is an error message 🚨");
//! warn!(&ctx, "this is a warning message ⚠️");
//! info!(&ctx, "this is an info message ℹ️");
//! debug!(&ctx, "this is a debug message ️🐝");
//! trace!(&ctx, "this is a trace message 🔍");
//! ```
//! Outputs:
//!
//! ```text
//! 21:58:55 [ERROR] xxx: [src/main.rs:35] this is an error message 🚨
//! 21:58:55 [WARN] xxx: [src/main.rs:36] this is a warning message ⚠️
//! 21:58:55 [INFO] xxx: [src/main.rs:37] this is an info message ℹ️
//! 21:58:55 [DEBUG] (7) xxx: [src/main.rs:38] this is a debug message ️🐝
//! 21:58:55 [TRACE] (7) xxx: [src/main.rs:39] this is a trace message 🔍
//! ```
//!
//! [Aya]: https://docs.rs/aya
//! [env_logger]: https://docs.rs/env_logger
//! [Log]: https://docs.rs/log/0.4.14/log/trait.Log.html
//! [log]: https://docs.rs/log
//!
use std::{
    fmt::{LowerHex, UpperHex},
    io, mem,
    net::{Ipv4Addr, Ipv6Addr},
    ptr, str,
    sync::Arc,
};

const MAP_NAME: &str = "AYA_LOGS";

use aya_log_common::{
    Argument, DisplayHint, Level, LogValueLength, RecordField, LOG_BUF_CAPACITY, LOG_FIELDS,
};
use bytes::BytesMut;
use log::{error, Log, Record};
use thiserror::Error;

use aya::{
    maps::{
        perf::{AsyncPerfEventArray, PerfBufferError},
        MapError,
    },
    util::online_cpus,
    Bpf, Pod,
};

#[derive(Copy, Clone)]
#[repr(transparent)]
struct RecordFieldWrapper(RecordField);
#[derive(Copy, Clone)]
#[repr(transparent)]
struct ArgumentWrapper(Argument);
#[derive(Copy, Clone)]
#[repr(transparent)]
struct DisplayHintWrapper(DisplayHint);

unsafe impl aya::Pod for RecordFieldWrapper {}
unsafe impl aya::Pod for ArgumentWrapper {}
unsafe impl aya::Pod for DisplayHintWrapper {}

/// Log messages generated by `aya_log_ebpf` using the [log] crate.
///
/// For more details see the [module level documentation](crate).
pub struct BpfLogger;

impl BpfLogger {
    /// Starts reading log records created with `aya-log-ebpf` and logs them
    /// with the default logger. See [log::logger].
    pub fn init(bpf: &mut Bpf) -> Result<BpfLogger, Error> {
        BpfLogger::init_with_logger(bpf, DefaultLogger {})
    }

    /// Starts reading log records created with `aya-log-ebpf` and logs them
    /// with the given logger.
    pub fn init_with_logger<T: Log + 'static>(
        bpf: &mut Bpf,
        logger: T,
    ) -> Result<BpfLogger, Error> {
        let logger = Arc::new(logger);
        let mut logs: AsyncPerfEventArray<_> = bpf
            .take_map(MAP_NAME)
            .ok_or(Error::MapNotFound)?
            .try_into()?;

        for cpu_id in online_cpus().map_err(Error::InvalidOnlineCpu)? {
            let mut buf = logs.open(cpu_id, None)?;

            let log = logger.clone();
            tokio::spawn(async move {
                let mut buffers = vec![BytesMut::with_capacity(LOG_BUF_CAPACITY); 10];

                loop {
                    let events = buf.read_events(&mut buffers).await.unwrap();

                    #[allow(clippy::needless_range_loop)]
                    for i in 0..events.read {
                        let buf = &mut buffers[i];
                        match log_buf(buf, &*log) {
                            Ok(()) => {}
                            Err(e) => error!("internal error => {:?}", e),
                        }
                    }
                }
            });
        }

        Ok(BpfLogger {})
    }
}

pub trait Formatter<T> {
    fn format(v: T) -> String;
}

pub struct DefaultFormatter;
impl<T> Formatter<T> for DefaultFormatter
where
    T: ToString,
{
    fn format(v: T) -> String {
        v.to_string()
    }
}

pub struct LowerHexFormatter;
impl<T> Formatter<T> for LowerHexFormatter
where
    T: LowerHex,
{
    fn format(v: T) -> String {
        format!("{v:x}")
    }
}

pub struct LowerHexDebugFormatter;
impl<T> Formatter<&[T]> for LowerHexDebugFormatter
where
    T: LowerHex,
{
    fn format(v: &[T]) -> String {
        let mut s = String::new();
        for v in v {
            let () = core::fmt::write(&mut s, format_args!("{v:x}")).unwrap();
        }
        s
    }
}

pub struct UpperHexFormatter;
impl<T> Formatter<T> for UpperHexFormatter
where
    T: UpperHex,
{
    fn format(v: T) -> String {
        format!("{v:X}")
    }
}

pub struct UpperHexDebugFormatter;
impl<T> Formatter<&[T]> for UpperHexDebugFormatter
where
    T: UpperHex,
{
    fn format(v: &[T]) -> String {
        let mut s = String::new();
        for v in v {
            let () = core::fmt::write(&mut s, format_args!("{v:X}")).unwrap();
        }
        s
    }
}

pub struct Ipv4Formatter;
impl<T> Formatter<T> for Ipv4Formatter
where
    T: Into<Ipv4Addr>,
{
    fn format(v: T) -> String {
        v.into().to_string()
    }
}

pub struct Ipv6Formatter;
impl<T> Formatter<T> for Ipv6Formatter
where
    T: Into<Ipv6Addr>,
{
    fn format(v: T) -> String {
        v.into().to_string()
    }
}

pub struct LowerMacFormatter;
impl Formatter<[u8; 6]> for LowerMacFormatter {
    fn format(v: [u8; 6]) -> String {
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            v[0], v[1], v[2], v[3], v[4], v[5]
        )
    }
}

pub struct UpperMacFormatter;
impl Formatter<[u8; 6]> for UpperMacFormatter {
    fn format(v: [u8; 6]) -> String {
        format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            v[0], v[1], v[2], v[3], v[4], v[5]
        )
    }
}

trait Format {
    fn format(&self, last_hint: Option<DisplayHintWrapper>) -> Result<String, ()>;
}

impl Format for &[u8] {
    fn format(&self, last_hint: Option<DisplayHintWrapper>) -> Result<String, ()> {
        match last_hint.map(|DisplayHintWrapper(dh)| dh) {
            Some(DisplayHint::LowerHex) => Ok(LowerHexDebugFormatter::format(self)),
            Some(DisplayHint::UpperHex) => Ok(UpperHexDebugFormatter::format(self)),
            _ => Err(()),
        }
    }
}

impl Format for u32 {
    fn format(&self, last_hint: Option<DisplayHintWrapper>) -> Result<String, ()> {
        match last_hint.map(|DisplayHintWrapper(dh)| dh) {
            Some(DisplayHint::Default) => Ok(DefaultFormatter::format(self)),
            Some(DisplayHint::LowerHex) => Ok(LowerHexFormatter::format(self)),
            Some(DisplayHint::UpperHex) => Ok(UpperHexFormatter::format(self)),
            Some(DisplayHint::Ipv4) => Ok(Ipv4Formatter::format(*self)),
            Some(DisplayHint::Ipv6) => Err(()),
            Some(DisplayHint::LowerMac) => Err(()),
            Some(DisplayHint::UpperMac) => Err(()),
            _ => Ok(DefaultFormatter::format(self)),
        }
    }
}

impl Format for [u8; 6] {
    fn format(&self, last_hint: Option<DisplayHintWrapper>) -> Result<String, ()> {
        match last_hint.map(|DisplayHintWrapper(dh)| dh) {
            Some(DisplayHint::Default) => Err(()),
            Some(DisplayHint::LowerHex) => Err(()),
            Some(DisplayHint::UpperHex) => Err(()),
            Some(DisplayHint::Ipv4) => Err(()),
            Some(DisplayHint::Ipv6) => Err(()),
            Some(DisplayHint::LowerMac) => Ok(LowerMacFormatter::format(*self)),
            Some(DisplayHint::UpperMac) => Ok(UpperMacFormatter::format(*self)),
            _ => Err(()),
        }
    }
}

impl Format for [u8; 16] {
    fn format(&self, last_hint: Option<DisplayHintWrapper>) -> Result<String, ()> {
        match last_hint.map(|DisplayHintWrapper(dh)| dh) {
            Some(DisplayHint::Default) => Err(()),
            Some(DisplayHint::LowerHex) => Err(()),
            Some(DisplayHint::UpperHex) => Err(()),
            Some(DisplayHint::Ipv4) => Err(()),
            Some(DisplayHint::Ipv6) => Ok(Ipv6Formatter::format(*self)),
            Some(DisplayHint::LowerMac) => Err(()),
            Some(DisplayHint::UpperMac) => Err(()),
            _ => Err(()),
        }
    }
}

impl Format for [u16; 8] {
    fn format(&self, last_hint: Option<DisplayHintWrapper>) -> Result<String, ()> {
        match last_hint.map(|DisplayHintWrapper(dh)| dh) {
            Some(DisplayHint::Default) => Err(()),
            Some(DisplayHint::LowerHex) => Err(()),
            Some(DisplayHint::UpperHex) => Err(()),
            Some(DisplayHint::Ipv4) => Err(()),
            Some(DisplayHint::Ipv6) => Ok(Ipv6Formatter::format(*self)),
            Some(DisplayHint::LowerMac) => Err(()),
            Some(DisplayHint::UpperMac) => Err(()),
            _ => Err(()),
        }
    }
}

macro_rules! impl_format {
    ($type:ident) => {
        impl Format for $type {
            fn format(&self, last_hint: Option<DisplayHintWrapper>) -> Result<String, ()> {
                match last_hint.map(|DisplayHintWrapper(dh)| dh) {
                    Some(DisplayHint::Default) => Ok(DefaultFormatter::format(self)),
                    Some(DisplayHint::LowerHex) => Ok(LowerHexFormatter::format(self)),
                    Some(DisplayHint::UpperHex) => Ok(UpperHexFormatter::format(self)),
                    Some(DisplayHint::Ipv4) => Err(()),
                    Some(DisplayHint::Ipv6) => Err(()),
                    Some(DisplayHint::LowerMac) => Err(()),
                    Some(DisplayHint::UpperMac) => Err(()),
                    _ => Ok(DefaultFormatter::format(self)),
                }
            }
        }
    };
}

impl_format!(i8);
impl_format!(i16);
impl_format!(i32);
impl_format!(i64);
impl_format!(isize);

impl_format!(u8);
impl_format!(u16);
impl_format!(u64);
impl_format!(usize);

macro_rules! impl_format_float {
    ($type:ident) => {
        impl Format for $type {
            fn format(&self, last_hint: Option<DisplayHintWrapper>) -> Result<String, ()> {
                match last_hint.map(|DisplayHintWrapper(dh)| dh) {
                    Some(DisplayHint::Default) => Ok(DefaultFormatter::format(self)),
                    Some(DisplayHint::LowerHex) => Err(()),
                    Some(DisplayHint::UpperHex) => Err(()),
                    Some(DisplayHint::Ipv4) => Err(()),
                    Some(DisplayHint::Ipv6) => Err(()),
                    Some(DisplayHint::LowerMac) => Err(()),
                    Some(DisplayHint::UpperMac) => Err(()),
                    _ => Ok(DefaultFormatter::format(self)),
                }
            }
        }
    };
}

impl_format_float!(f32);
impl_format_float!(f64);

#[derive(Copy, Clone, Debug)]
struct DefaultLogger;

impl Log for DefaultLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        log::logger().enabled(metadata)
    }

    fn log(&self, record: &Record) {
        log::logger().log(record)
    }

    fn flush(&self) {
        log::logger().flush()
    }
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("log event array {} doesn't exist", MAP_NAME)]
    MapNotFound,

    #[error("error opening log event array")]
    MapError(#[from] MapError),

    #[error("error opening log buffer")]
    PerfBufferError(#[from] PerfBufferError),

    #[error("invalid /sys/devices/system/cpu/online format")]
    InvalidOnlineCpu(#[source] io::Error),
}

fn log_buf(mut buf: &[u8], logger: &dyn Log) -> Result<(), ()> {
    let mut target = None;
    let mut level = None;
    let mut module = None;
    let mut file = None;
    let mut line = None;
    let mut num_args = None;

    for _ in 0..LOG_FIELDS {
        let (RecordFieldWrapper(tag), value, rest) = try_read(buf)?;

        match tag {
            RecordField::Target => {
                target = Some(str::from_utf8(value).map_err(|_| ())?);
            }
            RecordField::Level => {
                level = Some({
                    let level = unsafe { ptr::read_unaligned(value.as_ptr() as *const _) };
                    match level {
                        Level::Error => log::Level::Error,
                        Level::Warn => log::Level::Warn,
                        Level::Info => log::Level::Info,
                        Level::Debug => log::Level::Debug,
                        Level::Trace => log::Level::Trace,
                    }
                })
            }
            RecordField::Module => {
                module = Some(str::from_utf8(value).map_err(|_| ())?);
            }
            RecordField::File => {
                file = Some(str::from_utf8(value).map_err(|_| ())?);
            }
            RecordField::Line => {
                line = Some(u32::from_ne_bytes(value.try_into().map_err(|_| ())?));
            }
            RecordField::NumArgs => {
                num_args = Some(usize::from_ne_bytes(value.try_into().map_err(|_| ())?));
            }
        }

        buf = rest;
    }

    let mut full_log_msg = String::new();
    let mut last_hint: Option<DisplayHintWrapper> = None;
    for _ in 0..num_args.ok_or(())? {
        let (ArgumentWrapper(tag), value, rest) = try_read(buf)?;

        match tag {
            Argument::DisplayHint => {
                last_hint = Some(unsafe { ptr::read_unaligned(value.as_ptr() as *const _) });
            }
            Argument::I8 => {
                full_log_msg.push_str(
                    &i8::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::I16 => {
                full_log_msg.push_str(
                    &i16::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::I32 => {
                full_log_msg.push_str(
                    &i32::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::I64 => {
                full_log_msg.push_str(
                    &i64::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::Isize => {
                full_log_msg.push_str(
                    &isize::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::U8 => {
                full_log_msg.push_str(
                    &u8::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::U16 => {
                full_log_msg.push_str(
                    &u16::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::U32 => {
                full_log_msg.push_str(
                    &u32::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::U64 => {
                full_log_msg.push_str(
                    &u64::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::Usize => {
                full_log_msg.push_str(
                    &usize::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::F32 => {
                full_log_msg.push_str(
                    &f32::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::F64 => {
                full_log_msg.push_str(
                    &f64::from_ne_bytes(value.try_into().map_err(|_| ())?)
                        .format(last_hint.take())?,
                );
            }
            Argument::ArrU8Len6 => {
                let value: [u8; 6] = value.try_into().map_err(|_| ())?;
                full_log_msg.push_str(&value.format(last_hint.take())?);
            }
            Argument::ArrU8Len16 => {
                let value: [u8; 16] = value.try_into().map_err(|_| ())?;
                full_log_msg.push_str(&value.format(last_hint.take())?);
            }
            Argument::ArrU16Len8 => {
                let data: [u8; 16] = value.try_into().map_err(|_| ())?;
                let mut value: [u16; 8] = Default::default();
                for (i, s) in data.chunks_exact(2).enumerate() {
                    value[i] = ((s[1] as u16) << 8) | s[0] as u16;
                }
                full_log_msg.push_str(&value.format(last_hint.take())?);
            }
            Argument::Bytes => {
                full_log_msg.push_str(&value.format(last_hint.take())?);
            }
            Argument::Str => match str::from_utf8(value) {
                Ok(v) => {
                    full_log_msg.push_str(v);
                }
                Err(e) => error!("received invalid utf8 string: {}", e),
            },
        }

        buf = rest;
    }

    logger.log(
        &Record::builder()
            .args(format_args!("{full_log_msg}"))
            .target(target.ok_or(())?)
            .level(level.ok_or(())?)
            .module_path(module)
            .file(file)
            .line(line)
            .build(),
    );
    logger.flush();
    Ok(())
}

fn try_read<T: Pod>(mut buf: &[u8]) -> Result<(T, &[u8], &[u8]), ()> {
    if buf.len() < mem::size_of::<T>() + mem::size_of::<LogValueLength>() {
        return Err(());
    }

    let tag = unsafe { ptr::read_unaligned(buf.as_ptr() as *const T) };
    buf = &buf[mem::size_of::<T>()..];

    let len =
        LogValueLength::from_ne_bytes(buf[..mem::size_of::<LogValueLength>()].try_into().unwrap());
    buf = &buf[mem::size_of::<LogValueLength>()..];

    let len: usize = len.into();
    if buf.len() < len {
        return Err(());
    }

    let (value, rest) = buf.split_at(len);
    Ok((tag, value, rest))
}

#[cfg(test)]
mod test {
    use super::*;
    use aya_log_common::{write_record_header, WriteToBuf};
    use log::{logger, Level};

    fn new_log(args: usize) -> Result<(usize, Vec<u8>), ()> {
        let mut buf = vec![0; 8192];
        let len = write_record_header(
            &mut buf,
            "test",
            aya_log_common::Level::Info,
            "test",
            "test.rs",
            123,
            args,
        )?;
        Ok((len, buf))
    }

    #[test]
    fn test_str() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(1).unwrap();

        len += "test".write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "test");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_str_with_args() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(2).unwrap();

        len += "hello ".write(&mut input[len..]).unwrap();
        len += "test".write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "hello test");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_bytes() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(2).unwrap();

        len += DisplayHint::LowerHex.write(&mut input[len..]).unwrap();
        len += [0xde, 0xad].write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "dead");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_bytes_with_args() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(5).unwrap();

        len += DisplayHint::LowerHex.write(&mut input[len..]).unwrap();
        len += [0xde, 0xad].write(&mut input[len..]).unwrap();

        len += " ".write(&mut input[len..]).unwrap();

        len += DisplayHint::UpperHex.write(&mut input[len..]).unwrap();
        len += [0xbe, 0xef].write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "dead BEEF");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_display_hint_default() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(3).unwrap();

        len += "default hint: ".write(&mut input[len..]).unwrap();
        len += DisplayHint::Default.write(&mut input[len..]).unwrap();
        len += 14.write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "default hint: 14");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_display_hint_lower_hex() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(3).unwrap();

        len += "lower hex: ".write(&mut input[len..]).unwrap();
        len += DisplayHint::LowerHex.write(&mut input[len..]).unwrap();
        len += 200.write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "lower hex: c8");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_display_hint_upper_hex() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(3).unwrap();

        len += "upper hex: ".write(&mut input[len..]).unwrap();
        len += DisplayHint::UpperHex.write(&mut input[len..]).unwrap();
        len += 200.write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "upper hex: C8");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_display_hint_ipv4() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(3).unwrap();

        len += "ipv4: ".write(&mut input[len..]).unwrap();
        len += DisplayHint::Ipv4.write(&mut input[len..]).unwrap();
        // 10.0.0.1 as u32
        len += 167772161u32.write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "ipv4: 10.0.0.1");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_display_hint_ipv6_arr_u8_len_16() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(3).unwrap();

        len += "ipv6: ".write(&mut input[len..]).unwrap();
        len += DisplayHint::Ipv6.write(&mut input[len..]).unwrap();
        // 2001:db8::1:1 as byte array
        let ipv6_arr: [u8; 16] = [
            0x20, 0x01, 0x0d, 0xb8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
            0x00, 0x01,
        ];
        len += ipv6_arr.write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "ipv6: 2001:db8::1:1");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_display_hint_ipv6_arr_u16_len_8() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(3).unwrap();

        len += "ipv6: ".write(&mut input[len..]).unwrap();
        len += DisplayHint::Ipv6.write(&mut input[len..]).unwrap();
        // 2001:db8::1:1 as u16 array
        let ipv6_arr: [u16; 8] = [
            0x2001, 0x0db8, 0x0000, 0x0000, 0x0000, 0x0000, 0x0001, 0x0001,
        ];
        len += ipv6_arr.write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "ipv6: 2001:db8::1:1");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_display_hint_lower_mac() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(3).unwrap();

        len += "mac: ".write(&mut input[len..]).unwrap();
        len += DisplayHint::LowerMac.write(&mut input[len..]).unwrap();
        // 00:00:5e:00:53:af as byte array
        let mac_arr: [u8; 6] = [0x00, 0x00, 0x5e, 0x00, 0x53, 0xaf];
        len += mac_arr.write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "mac: 00:00:5e:00:53:af");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }

    #[test]
    fn test_display_hint_upper_mac() {
        testing_logger::setup();
        let (mut len, mut input) = new_log(3).unwrap();

        len += "mac: ".write(&mut input[len..]).unwrap();
        len += DisplayHint::UpperMac.write(&mut input[len..]).unwrap();
        // 00:00:5E:00:53:AF as byte array
        let mac_arr: [u8; 6] = [0x00, 0x00, 0x5e, 0x00, 0x53, 0xaf];
        len += mac_arr.write(&mut input[len..]).unwrap();

        _ = len;

        let logger = logger();
        let () = log_buf(&input, logger).unwrap();
        testing_logger::validate(|captured_logs| {
            assert_eq!(captured_logs.len(), 1);
            assert_eq!(captured_logs[0].body, "mac: 00:00:5E:00:53:AF");
            assert_eq!(captured_logs[0].level, Level::Info);
        });
    }
}
