extern crate mio;

use mio::tcp::*;
use mio::*;
use mio::util::Slab;
use std::thread;
use std::sync::mpsc::{self,channel};

const SERVER: mio::Token = mio::Token(0);

struct MiChat {
    listener: TcpListener,
    connections: Slab<Connection>,
    commands: mpsc::Sender<String>
}

impl MiChat {
    fn new(listener: TcpListener, commands: mpsc::Sender<String>) -> MiChat {
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
    commands: mpsc::Sender<String>,
}


impl Connection {
    fn new(socket: TcpStream, token: mio::Token, commands: mpsc::Sender<String>) -> Connection {
        Connection {
            socket: socket,
            token: token,
            state: Some(Vec::with_capacity(1024)),
            commands: commands,
        }
    }
    fn ready(&mut self, event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet) {
        println!("Connection::ready: {:?}; {:?}", self.socket.peer_addr(), events);
        if events.is_readable() {
            self.read(event_loop)
        }
    }

    fn read(&mut self, event_loop: &mut mio::EventLoop<MiChat>) {
        let mut abuf = Vec::new();
        match self.socket.try_read_buf(&mut abuf) {
            Ok(Some(0)) => {
                println!("{:?}: EOF!", self.socket.peer_addr());
                self.finish()
            },
            Ok(Some(n)) => {
                self.ingest(abuf);
                self.reregister(event_loop)
            },
            Ok(None) => {
                println!("{:?}: Noop!", self.socket.peer_addr());
                self.reregister(event_loop)
            },
            Err(e) => panic!("got an error trying to read; err={:?}", e)
        }
    }
    fn reregister(&mut self, event_loop: &mut mio::EventLoop<MiChat>) {
        event_loop.reregister(
                &self.socket,
                self.token,
                mio::EventSet::readable(),
                mio::PollOpt::oneshot()).unwrap()

    }
    fn ingest(&mut self, inbuf: Vec<u8>) {
        match self.state {
            Some(ref mut buf)  => {
                let mut startpos = 0;
                while let Some(n) = inbuf.iter().skip(startpos).position(|e| *e == '\n' as u8) {
                    Self::buffer_slice(buf, &inbuf[startpos..startpos+n]);
                    let s = String::from_utf8_lossy(buf).into_owned();
                    self.commands.send(s).unwrap();
                    startpos += n+1;
                    buf.clear()
                }
                println!("Remainder: {}", String::from_utf8_lossy(&inbuf[startpos..]));
                Self::buffer_slice(buf, &inbuf[startpos..])
            },
            None => panic!("Ingest on closed connection?")
        }
    }

    fn finish(&mut self) {
        match self.state {
            Some(ref mut buf)  => println!("Remainder: {}", String::from_utf8_lossy(buf)),
            _ => ()
        }
        self.state = None
    }

    fn buffer_slice(buf: &mut Vec<u8>, slice: &[u8]) {
        buf.extend(slice.iter().cloned());
    }

    fn is_closed(&self) -> bool {
        self.state.is_none()
    }
}

impl mio::Handler for MiChat {
    type Timeout = ();
    type Message = ();
    fn ready(&mut self, event_loop: &mut mio::EventLoop<Self>, token: mio::Token, events: mio::EventSet) {
        println!("{:?}: {:?}", token, events);
        match token {
            SERVER => {
                // Only receive readable events
                assert!(events.is_readable());

                println!("the listener socket is ready to accept a connection");
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
                        println!("the listener socket wasn't actually ready");
                    }
                    Err(e) => {
                        println!("listener.accept() errored: {}", e);
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
}

struct Model {
    count: usize
}

impl Model {
    fn new() -> Model {
        Model { count: 0 }
    }

    fn process_from(&mut self, rx: mpsc::Receiver<String>) {
        loop {
            let msg = rx.recv().unwrap();
            self.count += msg.len();
            println!("{}: {}; got {}", thread::current().name().unwrap_or("???"),
                    self.count, msg);
        }
    }
}

fn main() {
    let address = "0.0.0.0:6567".parse().unwrap();
    let listener = TcpListener::bind(&address).unwrap();

    let mut event_loop = mio::EventLoop::new().unwrap();
    event_loop.register(&listener, SERVER).unwrap();

    println!("running michat listener at: {:?}", address);

    let (tx, rx) = channel();
    let t = thread::Builder::new().name("Model".to_owned()).spawn(move|| {
        let mut model = Model::new();
        model.process_from(rx);
    }).unwrap();

    let mut service = MiChat::new(listener, tx);
    event_loop.run(&mut service).unwrap();
    t.join().unwrap();
}
