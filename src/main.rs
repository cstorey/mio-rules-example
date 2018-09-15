extern crate bytes;
extern crate env_logger;
extern crate mio;
#[macro_use]
extern crate log;
#[macro_use]
extern crate clap;

use clap::{App, Arg};
use mio::tcp::*;
use mio::util::Slab;
use mio::{TryRead, TryWrite};
use std::collections::VecDeque;
use std::ops;

#[derive(Debug)]
enum MiChatCommand {
    Broadcast(String),
    NewConnection(TcpStream),
}

struct MiChat {
    connections: Slab<EventHandler>,
}

trait RuleHandler {
    fn handle_event(&mut self, _event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet);
    fn register(&self, event_loop: &mut mio::EventLoop<MiChat>, token: mio::Token);
    fn process_rules(
        &mut self,
        event_loop: &mut mio::EventLoop<MiChat>,
        to_parent: &mut FnMut(MiChatCommand),
    );
    fn is_closed(&self) -> bool;
}

impl MiChat {
    fn new() -> MiChat {
        MiChat {
            connections: Slab::new(1024),
        }
    }

    fn listen(&mut self, event_loop: &mut mio::EventLoop<MiChat>, listener: TcpListener) {
        let l = EventHandler::Listener(Listener::new(listener));
        let token = self.connections.insert(l).expect("insert listener");
        &self.connections[token].register(event_loop, token);
    }

    fn process_action(&mut self, msg: MiChatCommand, event_loop: &mut mio::EventLoop<MiChat>) {
        trace!("{:p}; got {:?}", self, msg);
        match msg {
            MiChatCommand::Broadcast(s) => self.process_broadcast(s),
            MiChatCommand::NewConnection(socket) => self.process_new_connection(event_loop, socket),
        }
    }

    fn process_broadcast(&mut self, s: String) {
        for c in self.client_connections_mut() {
            c.enqueue(&s);
        }
    }

    fn process_new_connection(
        &mut self,
        event_loop: &mut mio::EventLoop<MiChat>,
        socket: mio::tcp::TcpStream,
    ) {
        let token = self
            .connections
            .insert_with(|token| EventHandler::Conn(Connection::new(socket, token)))
            .expect("token insert");
        &self.connections[token].register(event_loop, token);
    }

    fn client_connections_mut<'a>(&'a mut self) -> impl Iterator<Item = &'a mut Connection> + 'a {
        self.connections.iter_mut().filter_map(|it| match it {
            &mut EventHandler::Conn(ref mut c) => Some(c),
            _ => None,
        })
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

impl RuleHandler for Connection {
    fn register(&self, event_loop: &mut mio::EventLoop<MiChat>, token: mio::Token) {
        event_loop
            .register_opt(
                &self.socket,
                token,
                mio::EventSet::readable(),
                mio::PollOpt::edge() | mio::PollOpt::oneshot(),
            ).expect("event loop register");
    }

    // Event updates arrive here
    fn handle_event(&mut self, _event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet) {
        self.sock_status.insert(events);
        info!(
            "Connection::handle_event: {:?}; this time: {:?}; now: {:?}",
            self.socket.peer_addr(),
            events,
            self.sock_status
        );
    }

    // actions are processed here on down.

    fn process_rules(
        &mut self,
        event_loop: &mut mio::EventLoop<MiChat>,
        to_parent: &mut FnMut(MiChatCommand),
    ) {
        if self.sock_status.is_readable() {
            self.read();
            self.sock_status.remove(mio::EventSet::readable());
        }

        self.process_buffer(to_parent);

        if self.sock_status.is_writable() {
            self.write();
            self.sock_status.remove(mio::EventSet::writable());
        }

        if !self.is_closed() {
            self.reregister(event_loop)
        }
    }

    fn is_closed(&self) -> bool {
        self.failed || (self.read_eof && self.write_buf.is_empty())
    }
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
    fn process_buffer(&mut self, to_parent: &mut FnMut(MiChatCommand)) {
        let mut prev = 0;
        info!(
            "{:?}: Read buffer: {:?}",
            self.socket.peer_addr(),
            self.read_buf
        );
        for n in self.read_buf.iter().enumerate().filter_map(|(i, e)| {
            if *e == '\n' as u8 {
                Some(i)
            } else {
                None
            }
        }) {
            info!(
                "{:?}: Pos: {:?}; chunk: {:?}",
                self.socket.peer_addr(),
                n,
                &self.read_buf[prev..n]
            );
            let s = String::from_utf8_lossy(&self.read_buf[prev..n]).to_string();
            let cmd = MiChatCommand::Broadcast(s);
            info!("Send! {:?}", cmd);
            to_parent(cmd);
            prev = n + 1;
        }
        let remainder = self.read_buf[prev..].to_vec();
        info!(
            "{:?}: read Remainder: {}",
            self.socket.peer_addr(),
            remainder.len()
        );
        self.read_buf = remainder;
    }

    fn read(&mut self) {
        let mut abuf = Vec::new();
        match self.socket.try_read_buf(&mut abuf) {
            Ok(Some(0)) => {
                info!("{:?}: EOF!", self.socket.peer_addr());
                self.read_eof = true
            }
            Ok(Some(n)) => {
                info!("{:?}: Read {}bytes", self.socket.peer_addr(), n);
                self.read_buf.extend(abuf);
            }
            Ok(None) => {
                info!("{:?}: Noop!", self.socket.peer_addr());
            }
            Err(e) => {
                error!("got an error trying to read; err={:?}", e);
                self.failed = true;
            }
        }
    }

    fn write(&mut self) {
        match self.socket.try_write(&mut self.write_buf) {
            Ok(Some(n)) => {
                info!(
                    "{:?}: Wrote {} of {} in buffer",
                    self.socket.peer_addr(),
                    n,
                    self.write_buf.len()
                );
                self.write_buf = self.write_buf[n..].to_vec();
                info!(
                    "{:?}: Now {:?}b",
                    self.socket.peer_addr(),
                    self.write_buf.len()
                );
            }
            Ok(None) => {
                info!("Write unready");
            }
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

        event_loop
            .reregister(&self.socket, self.token, flags, mio::PollOpt::oneshot())
            .expect("EventLoop#reregister")
    }

    fn enqueue(&mut self, s: &str) {
        self.write_buf.extend(s.as_bytes());
        self.write_buf.push('\n' as u8);
    }
}

#[derive(Debug)]
struct Listener {
    listener: TcpListener,
    sock_status: mio::EventSet,
}

impl Listener {
    fn new(listener: TcpListener) -> Listener {
        Listener {
            listener: listener,
            sock_status: mio::EventSet::none(),
        }
    }
}

impl RuleHandler for Listener {
    fn register(&self, event_loop: &mut mio::EventLoop<MiChat>, token: mio::Token) {
        event_loop
            .register(&self.listener, token)
            .expect("Register listener");
    }

    fn handle_event(&mut self, _event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet) {
        assert!(events.is_readable());
        self.sock_status.insert(events);
        info!(
            "Listener::handle_event: {:?}; this time: {:?}; now: {:?}",
            self.listener.local_addr(),
            events,
            self.sock_status
        );
    }
    fn process_rules(
        &mut self,
        event_loop: &mut mio::EventLoop<MiChat>,
        to_parent: &mut FnMut(MiChatCommand),
    ) {
        if self.sock_status.is_readable() {
            info!("the listener socket is ready to accept a connection");
            match self.listener.accept() {
                Ok(Some(socket)) => {
                    let cmd = MiChatCommand::NewConnection(socket);
                    to_parent(cmd);
                }
                Ok(None) => {
                    info!("the listener socket wasn't actually ready");
                }
                Err(e) => {
                    info!("listener.accept() errored: {}", e);
                    event_loop.shutdown();
                }
            }
            self.sock_status.remove(mio::EventSet::readable());
        }
    }

    fn is_closed(&self) -> bool {
        false
    }
}

#[derive(Debug)]
enum EventHandler {
    Listener(Listener),
    Conn(Connection),
}

impl ops::Deref for EventHandler {
    type Target = RuleHandler;
    fn deref(&self) -> &Self::Target {
        match self {
            &EventHandler::Conn(ref conn) => conn,
            &EventHandler::Listener(ref listener) => listener,
        }
    }
}
impl ops::DerefMut for EventHandler {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            &mut EventHandler::Conn(ref mut conn) => conn,
            &mut EventHandler::Listener(ref mut listener) => listener,
        }
    }
}

impl mio::Handler for MiChat {
    type Timeout = ();
    type Message = ();
    fn ready(
        &mut self,
        event_loop: &mut mio::EventLoop<Self>,
        token: mio::Token,
        events: mio::EventSet,
    ) {
        info!("{:?}: {:?}", token, events);
        self.connections[token].handle_event(event_loop, events);
        if self.connections[token].is_closed() {
            info!("Removing; {:?}", token);
            self.connections.remove(token);
        }

        let mut parent_actions = VecDeque::new();
        loop {
            for conn in self.connections.iter_mut() {
                conn.process_rules(event_loop, &mut |action| parent_actions.push_back(action));
            }
            // Anything left to process?
            if parent_actions.is_empty() {
                break;
            }

            for action in parent_actions.drain(..) {
                self.process_action(action, event_loop);
            }
        }
    }
}

fn main() {
    env_logger::init();
    let matches = App::new("michat")
        .arg(
            Arg::with_name("bind")
                .short("l")
                .takes_value(true)
                .required(true),
        ).get_matches();

    let address = value_t_or_exit!(matches.value_of("bind"), std::net::SocketAddr);
    let listener = TcpListener::bind(&address).expect("bind");

    let mut event_loop = mio::EventLoop::new().expect("Create event loop");
    let mut service = MiChat::new();

    service.listen(&mut event_loop, listener);
    info!("running michat listener at: {:?}", address);
    event_loop.run(&mut service).expect("Run loop");
}
