extern crate mio;

use mio::tcp::*;
use mio::*;
use mio::util::Slab;

const SERVER: mio::Token = mio::Token(0);

struct MiChat {
    listener: TcpListener,
    connections: Slab<Connection>,
}

#[derive(Debug)]
struct Connection {
    socket: TcpStream,
    token: mio::Token,
    state: Option<Vec<u8>>,
}


impl Connection {
    fn new(socket: TcpStream, token: mio::Token) -> Connection {
        Connection {
            socket: socket,
            token: token,
            state: Some(Vec::with_capacity(1024))
        }
    }
    fn ready(&mut self, event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet) {
        println!("Connection::ready: {:?}; {:?}", self, events);
        if events.is_readable() {
            self.read(event_loop)
        }
    }

    fn read(&mut self, event_loop: &mut mio::EventLoop<MiChat>) {
        let mut abuf = Vec::new();
        match self.socket.try_read_buf(&mut abuf) {
            Ok(Some(0)) => {
                println!("{:?}: EOF!", self);
                self.finish()
            },
            Ok(Some(n)) => {
                println!("Read so far: {:?}", n);
                self.ingest(abuf);
                self.reregister(event_loop)
            },
            Ok(None) => {
                println!("{:?}: Noop!", self);
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
                    println!("Newline from {} @{}", startpos, n);
                    Self::buffer_slice(buf, &inbuf[startpos..startpos+n]);
                    println!("Line: {}", String::from_utf8_lossy(buf));
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
                        println!("accepted a socket, exiting program");
                        let token = self.connections
                            .insert_with(|token| Connection::new(socket, token))
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

fn main() {
    let address = "0.0.0.0:6567".parse().unwrap();
    let listener = TcpListener::bind(&address).unwrap();

    let mut event_loop = mio::EventLoop::new().unwrap();
    event_loop.register(&listener, SERVER).unwrap();

    println!("running michat listener at: {:?}", address);
    let mut service = MiChat { listener: listener, connections: Slab::new_starting_at(mio::Token(1), 1024) };
    event_loop.run(&mut service).unwrap();
}
