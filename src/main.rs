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
use clap::{Arg, App, SubCommand};

use bytes::{Buf, Take};

const SERVER: mio::Token = mio::Token(0);

#[derive(Debug)]
enum MiChatCommand {
    Set(String),
    Get(mio::Token),
}

#[derive(Debug)]
struct MiChatIoEvent (mio::Token, String);

struct MiChat {
    listener: TcpListener,
    connections: Slab<Connection>,
    commands: mpsc::Sender<MiChatCommand>
}

impl MiChat {
    fn new(listener: TcpListener, commands: mpsc::Sender<MiChatCommand>) -> MiChat {
        MiChat {
            listener: listener,
            connections: Slab::new_starting_at(mio::Token(1), 1024),
            commands: commands
        }
    }
}

struct Connection {
    socket: TcpStream,
    sock_status: mio::EventSet,
    token: mio::Token,
    read_buf: Vec<u8>,
    read_eof: bool,
    failed: bool,
    write_buf: Vec<u8>,
    commands: mpsc::Sender<MiChatCommand>,
}


impl Connection {
    fn new(socket: TcpStream, token: mio::Token, commands: mpsc::Sender<MiChatCommand>) -> Connection {
        Connection {
            socket: socket,
            sock_status: mio::EventSet::none(),
            token: token,
            read_buf: Vec::with_capacity(1024),
            write_buf: Vec::new(),
            read_eof: false,
            failed: false,
            commands: commands,
        }
    }
    fn ready(&mut self, event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet) {
        self.sock_status.insert(events);
        info!("Connection::ready: {:?}; this time: {:?}; now: {:?}",
                self.socket.peer_addr(), events, self.sock_status);
        if self.sock_status.is_readable() {
            self.read(event_loop);
            self.sock_status.remove(mio::EventSet::readable());
        }

        self.process();

        if self.sock_status.is_writable() {
            self.write(event_loop);
            self.sock_status.remove(mio::EventSet::writable());
        }

        if !self.is_closed() {
            self.reregister(event_loop)
        }
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

    fn notify(&mut self, event_loop: &mut mio::EventLoop<MiChat>, s: String) {
        self.write_buf.extend(s.bytes());
        self.write_buf.push('\n' as u8);
        self.write(event_loop);
        self.reregister(event_loop);
    }

    fn write(&mut self, event_loop: &mut mio::EventLoop<MiChat>) {
        match self.socket.try_write(&mut self.write_buf) {
            Ok(Some(n)) => {
                info!("Wrote {} of {} in buffer", n, self.write_buf.len());
                self.write_buf = self.write_buf[n..].to_vec();
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

        event_loop.reregister(
                &self.socket,
                self.token,
                flags,
                mio::PollOpt::oneshot()).expect("EventLoop#reregister")

    }
    fn process(&mut self) {
        let mut prev = 0;
        for n in self.read_buf.iter().enumerate()
                .filter_map(|(i, e)| if *e == '\n' as u8 { Some(i) } else { None } ) {
            let s = String::from_utf8_lossy(&self.read_buf[prev..n]).into_owned();
            let cmd = if s.is_empty() { MiChatCommand::Get(self.token) } else { MiChatCommand::Set(s) };
            info!("Send! {:?}", cmd);
            self.commands.send(cmd).expect("Sender#send");
            prev = n;
        }
        let remainder = self.read_buf[prev..].to_vec();
        info!("Remainder: {}", String::from_utf8_lossy(&remainder));
        self.read_buf = remainder;
    }

    fn buffer_slice(buf: &mut Vec<u8>, slice: &[u8]) {
        buf.extend(slice.iter().cloned());
    }

    fn is_closed(&self) -> bool {
        self.failed || (self.read_eof && self.write_buf.is_empty())
    }
}

impl mio::Handler for MiChat {
    type Timeout = ();
    type Message = MiChatIoEvent;
    fn ready(&mut self, event_loop: &mut mio::EventLoop<Self>, token: mio::Token, events: mio::EventSet) {
        info!("{:?}: {:?}", token, events);
        match token {
            SERVER => {
                // Only receive readable events
                assert!(events.is_readable());

                info!("the listener socket is ready to accept a connection");
                match self.listener.accept() {
                    Ok(Some(socket)) => {
                        let commands = self.commands.clone();
                        let token = self.connections
                            .insert_with(|token| Connection::new(socket, token, commands))
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
                if self.connections[token].is_closed() {
                    info!("Removing; {:?}", token);
                    self.connections.remove(token);
                }
            }
        }
    }

    fn notify(&mut self, event_loop: &mut EventLoop<Self>, msg: MiChatIoEvent) {
        info!("Notify: {:?}", msg);
        let MiChatIoEvent(token, s) = msg;
        if let Some(conn) = self.connections.get_mut(token) {
            conn.notify(event_loop, s);
        } else {
            warn!("No connection for token: {:?}", token);
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
            info!("{:?}; got {:?}", self, msg);
            match msg {
                MiChatCommand::Set(s) => {
                    self.value = s;
                },
                MiChatCommand::Get(tok) => {
                    while let Err(e) = replies.send(MiChatIoEvent(tok, self.value.clone())) {
                        info!("Backoff on notify: {:?}", e);
                        thread::sleep_ms(10);
                    }
                }
            }
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

    let (command_tx, command_rx) = channel();
    let replies = event_loop.channel();
    let t = thread::Builder::new().name("Model".to_owned()).spawn(move|| {
        let mut model = Model::new();
        model.process_from(command_rx, replies);
    }).expect("spawn model thread");

    let mut service = MiChat::new(listener, command_tx);
    event_loop.run(&mut service).expect("Run loop");
    t.join().expect("Model join");
}
