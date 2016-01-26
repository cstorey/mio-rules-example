extern crate mio;
extern crate bytes;
extern crate log4rs;
#[macro_use]
extern crate log;
#[macro_use]
extern crate clap;

use mio::tcp::*;
use mio::{Sender,EventLoop};
use mio::{TryRead,TryWrite};
use mio::util::Slab;
use std::thread;
use std::sync::mpsc::{self,channel};
use std::io::Cursor;
use std::collections::VecDeque;
use clap::{Arg, App, SubCommand};

use bytes::{Buf, Take};

const SERVER: mio::Token = mio::Token(0);

#[derive(Debug)]
enum MiChatCommand {
    Broadcast(String),
}

#[derive(Debug)]
struct MiChatIoEvent (mio::Token, String);

struct MiChat {
    listener: TcpListener,
    connections: Slab<Connection>,
    register: String,
}

impl MiChat {
    fn new(listener: TcpListener) -> MiChat {
        MiChat {
            listener: listener,
            connections: Slab::new_starting_at(mio::Token(1), 1024),
            register: String::new(),
        }
    }
    fn process_action(&mut self, msg: &MiChatCommand) {
        trace!("{:p}; got {:?}", self, msg);
        match msg {
            &MiChatCommand::Broadcast(ref s) => {
                for c in self.connections.iter_mut() {
                    c.enqueue(&s);
                }
            }
        }
    }
}

#[derive(Debug)]
struct Connection {
    socket: TcpStream,
    sock_status: mio::EventSet,
    token: mio::Token,
    read_buf: Vec<u8>,
    read_eof: bool,
    failed: bool,
    write_buf: Vec<u8>,
}


impl Connection {
    fn new(socket: TcpStream, token: mio::Token) -> Connection {
        Connection {
            socket: socket,
            sock_status: mio::EventSet::none(),
            token: token,
            read_buf: Vec::with_capacity(1024),
            write_buf: Vec::new(),
            read_eof: false,
            failed: false,
        }
    }
    // Event updates arrive here
    fn ready(&mut self, event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet) {
        self.sock_status.insert(events);
        info!("Connection::ready: {:?}; this time: {:?}; now: {:?}",
                self.socket.peer_addr(), events, self.sock_status);
    }

    // actions are processed here on down.

    fn process_rules(&mut self, event_loop: &mut mio::EventLoop<MiChat>,
        to_parent: &mut VecDeque<MiChatCommand>) {
        if self.sock_status.is_readable() {
            self.read(event_loop);
            self.sock_status.remove(mio::EventSet::readable());
        }

        self.process_buffer(to_parent);

        if self.sock_status.is_writable() {
            self.write(event_loop);
            self.sock_status.remove(mio::EventSet::writable());
        }

        if !self.is_closed() {
            self.reregister(event_loop)
        }
    }

    fn process_buffer(&mut self, to_parent: &mut VecDeque<MiChatCommand>) {
        let mut prev = 0;
        info!("{:?}: Read buffer: {:?}", self.socket.peer_addr(), self.read_buf);
        for n in self.read_buf.iter().enumerate()
                .filter_map(|(i, e)| if *e == '\n' as u8 { Some(i) } else { None } ) {
            info!("{:?}: Pos: {:?}; chunk: {:?}", self.socket.peer_addr(), n, &self.read_buf[prev..n]);
            let s = String::from_utf8_lossy(&self.read_buf[prev..n]).to_string();
            let cmd = MiChatCommand::Broadcast(s);
            info!("Send! {:?}", cmd);
            to_parent.push_back(cmd);
            prev = n+1;
        }
        let remainder = self.read_buf[prev..].to_vec();
        info!("{:?}: read Remainder: {}", self.socket.peer_addr(), remainder.len());
        self.read_buf = remainder;
    }

    fn read(&mut self, event_loop: &mut mio::EventLoop<MiChat>) {
        let mut abuf = Vec::new();
        match self.socket.try_read_buf(&mut abuf) {
            Ok(Some(0)) => {
                info!("{:?}: EOF!", self.socket.peer_addr());
                self.read_eof = true
            },
            Ok(Some(n)) => {
                info!("{:?}: Read {}bytes", self.socket.peer_addr(), n);
                self.read_buf.extend(abuf);
            },
            Ok(None) => {
                info!("{:?}: Noop!", self.socket.peer_addr());
            },
            Err(e) => {
                error!("got an error trying to read; err={:?}", e);
                self.failed =true;
            }
        }
    }

    fn write(&mut self, event_loop: &mut mio::EventLoop<MiChat>) {
        match self.socket.try_write(&mut self.write_buf) {
            Ok(Some(n)) => {
                info!("{:?}: Wrote {} of {} in buffer ({:?}...)", self.socket.peer_addr(), n,
                    self.write_buf.len(), &self.write_buf[0..2]);
                self.write_buf = self.write_buf[n..].to_vec();
                info!("{:?}: Now {:?}b", self.socket.peer_addr(), self.write_buf.len());
            },
            Ok(None) => {
                info!("Write unready");
            },
            Err(e) => {
                error!("got an error trying to write; err={:?}", e);
                self.failed = true;
            }
        }
    }

    fn reregister(&mut self, event_loop: &mut mio::EventLoop<MiChat>) {
        let mut flags = mio::EventSet::readable();
        if !self.write_buf.is_empty() {
            flags.insert(mio::EventSet::writable());
        }
        info!("Registering {:?} with {:?}", self, flags);

        event_loop.reregister(
                &self.socket,
                self.token,
                flags,
                mio::PollOpt::oneshot()).expect("EventLoop#reregister")
    }



    fn buffer_slice(buf: &mut Vec<u8>, slice: &[u8]) {
        buf.extend(slice.iter().cloned());
    }

    fn is_closed(&self) -> bool {
        self.failed || (self.read_eof && self.write_buf.is_empty())
    }

    fn enqueue(&mut self, s: &str) {
        self.write_buf.extend(s.as_bytes());
        self.write_buf.push('\n' as u8);
    }
}

impl mio::Handler for MiChat {
    type Timeout = ();
    type Message = ();
    fn ready(&mut self, event_loop: &mut mio::EventLoop<Self>, token: mio::Token, events: mio::EventSet) {
        info!("{:?}: {:?}", token, events);
        let mut next = VecDeque::new();

        match token {
            SERVER => {
                // Only receive readable events
                assert!(events.is_readable());

                info!("the listener socket is ready to accept a connection");
                match self.listener.accept() {
                    Ok(Some(socket)) => {
                        let token = self.connections
                            .insert_with(|token| Connection::new(socket, token))
                            .expect("token insert");

                        event_loop.register_opt(
                            &self.connections[token].socket,
                            token,
                            mio::EventSet::readable(),
                            mio::PollOpt::edge() | mio::PollOpt::oneshot())
                        .expect("event loop register");
                    }
                    Ok(None) => {
                        info!("the listener socket wasn't actually ready");
                    }
                    Err(e) => {
                        info!("listener.accept() errored: {}", e);
                        event_loop.shutdown();
                    }
                }
            }
            _ => {
                self.connections[token].ready(event_loop, events);
                info!("TODO next on {:?}: {:?}", token, next);
                if self.connections[token].is_closed() {
                    info!("Removing; {:?}", token);
                    self.connections.remove(token);
                }
            }
        }

        loop {
            for conn in self.connections.iter_mut() {
                conn.process_rules(event_loop, &mut next);
            }
            if next.is_empty() { break; }

            for action in next.iter() {
                self.process_action(action);
            }
            next.clear();
        }
    }
}

#[derive(Debug)]
struct Model {
    value: String
}

impl Model {
    fn new() -> Model {
        Model { value: "".to_string() }
    }

    fn process_from(&mut self, rx: mpsc::Receiver<MiChatCommand>, replies: Sender<MiChatIoEvent>) {
        loop {
            let msg = rx.recv().expect("Model receive");

        }
    }
}

const LOG_FILE: &'static str = "log.toml";

fn main() {
    if let Err(e) = log4rs::init_file(LOG_FILE, Default::default()) {
        panic!("Could not init logger from file {}: {}", LOG_FILE, e);
    }
    let matches = App::new("michat")
        .arg(Arg::with_name("bind").short("l").takes_value(true).required(true))
        .get_matches();

    let address = value_t_or_exit!(matches.value_of("bind"), std::net::SocketAddr);
    let listener = TcpListener::bind(&address).expect("bind");

    let mut event_loop = mio::EventLoop::new().expect("Create event loop");
    event_loop.register(&listener, SERVER).expect("Register listener");

    info!("running michat listener at: {:?}", address);

    let mut service = MiChat::new(listener);
    event_loop.run(&mut service).expect("Run loop");
}
