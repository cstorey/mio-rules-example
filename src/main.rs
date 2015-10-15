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
    token: mio::Token,
    state: Option<Vec<u8>>,
    write_buf: Vec<u8>,
    commands: mpsc::Sender<MiChatCommand>,
}


impl Connection {
    fn new(socket: TcpStream, token: mio::Token, commands: mpsc::Sender<MiChatCommand>) -> Connection {
        Connection {
            socket: socket,
            token: token,
            state: Some(Vec::with_capacity(1024)),
            write_buf: Vec::new(),
            commands: commands,
        }
    }
    fn ready(&mut self, event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet) {
        info!("Connection::ready: {:?}; {:?}", self.socket.peer_addr(), events);
        if events.is_readable() {
            self.read(event_loop)
        }
        if events.is_writable() {
            self.write(event_loop)
        }
    }

    fn read(&mut self, event_loop: &mut mio::EventLoop<MiChat>) {
        let mut abuf = Vec::new();
        match self.socket.try_read_buf(&mut abuf) {
            Ok(Some(0)) => {
                info!("{:?}: EOF!", self.socket.peer_addr());
                self.finish()
            },
            Ok(Some(n)) => {
                info!("{:?}: Read {}bytes", self.socket.peer_addr(), n);
                self.ingest(abuf);
                self.reregister(event_loop)
            },
            Ok(None) => {
                info!("{:?}: Noop!", self.socket.peer_addr());
                self.reregister(event_loop)
            },
            Err(e) => panic!("got an error trying to read; err={:?}", e)
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
                self.reregister(event_loop);
            },
            Ok(None) => {
                info!("Write unready");
                self.reregister(event_loop);
            },
            Err(e) => {
                panic!("got an error trying to write; err={:?}", e);
            }
        }
    }

    fn reregister(&mut self, event_loop: &mut mio::EventLoop<MiChat>) {
        let mut flags = mio::EventSet::readable();
        if !self.write_buf.is_empty() {
            flags = flags | mio::EventSet::writable();
        }

        event_loop.reregister(
                &self.socket,
                self.token,
                flags,
                mio::PollOpt::oneshot()).unwrap()

    }
    fn ingest(&mut self, inbuf: Vec<u8>) {
        match self.state {
            Some(ref mut buf)  => {
                let mut startpos = 0;
                while let Some(n) = inbuf.iter().skip(startpos).position(|e| *e == '\n' as u8) {
                    Self::buffer_slice(buf, &inbuf[startpos..startpos+n]);
                    let s = String::from_utf8_lossy(buf).into_owned();
                    let cmd = if s.is_empty() { MiChatCommand::Get(self.token) } else { MiChatCommand::Set(s) };
                    self.commands.send(cmd).unwrap();
                    startpos += n+1;
                    buf.clear()
                }
                info!("Remainder: {}", String::from_utf8_lossy(&inbuf[startpos..]));
                Self::buffer_slice(buf, &inbuf[startpos..])
            },
            None => panic!("Ingest on closed connection?")
        }
    }

    fn finish(&mut self) {
        match self.state {
            Some(ref mut buf)  => info!("Remainder: {}", String::from_utf8_lossy(buf)),
            _ => ()
        }
        self.state = None

    }

    fn buffer_slice(buf: &mut Vec<u8>, slice: &[u8]) {
        buf.extend(slice.iter().cloned());
    }

    fn is_closed(&self) -> bool {
        self.state.is_none() && self.write_buf.is_empty()
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
                            .unwrap();

                        event_loop.register_opt(
                            &self.connections[token].socket,
                            token,
                            mio::EventSet::readable(),
                            mio::PollOpt::edge() | mio::PollOpt::oneshot()).unwrap();
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
            let msg = rx.recv().unwrap();
            info!("{:?}; got {:?}", self, msg);
            match msg {
                MiChatCommand::Set(s) => {
                    self.value = s;
                },
                MiChatCommand::Get(tok) => {
                    replies.send(MiChatIoEvent(tok, self.value.clone())).unwrap();
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
    let listener = TcpListener::bind(&address).unwrap();

    let mut event_loop = mio::EventLoop::new().unwrap();
    event_loop.register(&listener, SERVER).unwrap();

    info!("running michat listener at: {:?}", address);

    let (command_tx, command_rx) = channel();
    let replies = event_loop.channel();
    let t = thread::Builder::new().name("Model".to_owned()).spawn(move|| {
        let mut model = Model::new();
        model.process_from(command_rx, replies);
    }).unwrap();

    let mut service = MiChat::new(listener, command_tx);
    event_loop.run(&mut service).unwrap();
    t.join().unwrap();
}
