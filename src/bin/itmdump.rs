#![deny(warnings)]
#![feature(conservative_impl_trait)]

extern crate chrono;
extern crate clap;
extern crate env_logger;
#[macro_use]
extern crate error_chain;
extern crate heapless;
extern crate libc;
#[macro_use]
extern crate log;
extern crate ref_slice;


use clap::{Arg, App, ArgMatches};
use heapless::Vec as HVec;
use log::{LogRecord, LogLevelFilter};
use std::fs::File;
use std::io::{Read, Write};
use std::time::Duration;
use std::{env, io, process, thread};

use errors::{Error, ErrorKind, Result, ResultExt};

mod errors {
    error_chain! {
        foreign_links {
            Io(::std::io::Error);
        }

        errors {
            UnknownHeader(b: u8) {
                description("unknown header byte"),
                display("unknown header byte: {:x}", b),
            }
        }
    }
}

pub const MAX_PAYLOAD_SIZE: usize = 4;

// TODO: Probably add a .kind field and Kind enum when we need to handle more
// kinds of packets.
struct Packet {
    // The header byte received for this packet.
    pub header: u8,

    /// Data in this packet.
    pub payload: HVec<u8, [u8; MAX_PAYLOAD_SIZE]>,

    /// Stimuls port this packet was sent from.
    pub port: u8,
}

fn main() {
    // Initialise logging.
    env_logger::LogBuilder::new()
        .format(|r: &LogRecord|
            format!("\n{time} {lvl} {mod} @{file}:{line}\n  {args}",
                time = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f_UTC"),
                lvl = r.level(),
                mod = r.location().module_path(),
                file = r.location().file(),
                line = r.location().line(),
                args = r.args())
        )
        .filter(None, LogLevelFilter::Info)
        .parse(&env::var("RUST_LOG").unwrap_or(String::from("")))
        .init().unwrap();

    fn show_backtrace() -> bool {
        env::var("RUST_BACKTRACE").as_ref().map(|s| &s[..]) == Ok("1")
    }

    if let Err(e) = run() {
        let stderr = io::stderr();
        let mut stderr = stderr.lock();

        writeln!(stderr, "{}", e).unwrap();

        for e in e.iter().skip(1) {
            writeln!(stderr, "caused by: {}", e).unwrap();
        }

        if show_backtrace() {
            if let Some(backtrace) = e.backtrace() {
                writeln!(stderr, "{:?}", backtrace).unwrap();
            }
        }

        process::exit(1)
    }
}

fn run() -> Result<()> {
    let matches = App::new("itmdump")
        .version(include_str!(concat!(env!("OUT_DIR"), "/commit-info.txt")))
        .about("\n\
                Reads data from an ARM CPU ITM and decodes it. \n\
                \n\
                Input is from an existing file (or named pipe) at a \
                supplied path, or else from standard input.")
        .arg(Arg::with_name("file")
                 .long("file")
                 .short("f")
                 .help("Path to file (or named pipe) to read from")
                 .takes_value(true))
        .arg(Arg::with_name("follow")
                 .long("follow")
                 .short("F")
                 .help("Keep the file open after reading through it and \
                        append new output as it is written. Like `tail -f'."))
        .arg(Arg::with_name("port")
                 .long("stimulus")
                 .short("s")
                 .help("Stimulus port to extract ITM data for.")
                 .takes_value(true)
                 .default_value("0")
                 .validator(|s| match s.parse::<u8>() {
                                    Ok(_) => Ok(()),
                                    Err(e) => Err(e.to_string())
                                }))
        .get_matches();

    let port = matches.value_of("port")
                           .unwrap() // We supplied a default value
                           .parse::<u8>()
                           .expect("Arg validator should ensure this parses");

    let follow = matches.is_present("follow");

    let mut stream = open_read(&matches)?;

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    loop {
        let p = read_packet(&mut stream);
        match p {
            Ok(p) => {
                if p.port == port {
                    stdout.write_all(&p.payload)?;
                }
            }
            Err(e @ Error(ErrorKind::UnknownHeader(_), _)) => {
                // We don't know this header type; warn and continue.
                debug!("{}", e);
            },
            Err(Error(ErrorKind::Io(ref e), _))
            if e.kind() == io::ErrorKind::UnexpectedEof => {
                if follow {
                    // TODO: There's a bug here where we can lose
                    // data.  UnexpectedEof is returned when
                    // read_exact() encounters EOF before it fills its
                    // buffer, but in that case it may have already
                    // read _some_ data, which we discard here.
                    //
                    // Instead we could buffer input until we can read
                    // a full packet, or turn parsing into a state
                    // machine.
                    thread::sleep(Duration::from_millis(100));
                } else {
                    // !follow and EOF. Exit.
                    return Ok(())
                }
            },
            Err(e) => return Err(e),
        }
    } // end of read loop

    // Unreachable.
}

fn open_read<'a>(matches: &ArgMatches) -> Result<impl io::Read + 'a> {
    let path = matches.value_of("file");
    Ok(match path {
        Some(path) => {
            let f =
                File::open(path)
                     .chain_err(|| format!("Couldn't open source file '{}'",
                                           path))?;
            Box::new(f) as Box<io::Read + 'static>
        },
        None => Box::new(io::stdin()) as Box<io::Read + 'static>,
    })
}

fn read_packet(input: &mut Read) -> Result<Packet> {
    let mut header = [0; 1];
    input.read_exact(&mut header)?;
    let header = header[0];
    let mut packet = Packet {
        header: header,
        payload: HVec::new(),
        port: header >> 3,
    };
    match header & 0b111 {
        0b01|0b10|0b11 => {
            // Data packet.
            let payload_size =
                match header & 0b11 {
                    0b01 => 1,
                    0b10 => 2,
                    0b11 => 4,
                    _ => return Err(Error::from(
                                    ErrorKind::UnknownHeader(header))),
                };
            // TODO: payload.resize_default(), would be nice.
            for _ in 0..payload_size {
                packet.payload.push(0)
                      .expect("payload_size <= packet.payload.capacity");
            }
            input.read_exact(&mut *packet.payload)?;
            Ok(packet)
        },
        _ => {
            return Err(Error::from(ErrorKind::UnknownHeader(header)));
        }
    }
}

// TODO: Add parse tests.
