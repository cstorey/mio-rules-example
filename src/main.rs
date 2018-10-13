extern crate bytes;
extern crate env_logger;
extern crate mio;
#[macro_use]
extern crate log;
#[macro_use]
extern crate clap;
extern crate failure;
extern crate slab;

use clap::{App, Arg};
use failure::Error;
use failure::ResultExt;
use mio::net::{TcpListener, TcpStream};
use slab::Slab;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::ops;

#[derive(Debug)]
enum MiChatCommand {
    Broadcast(String),
    NewConnection(TcpStream),
    CloseConnection,
}

struct MiChat {
    connections: Slab<EventHandler>,
}

trait RuleHandler {
    fn handle_event(&mut self, _event_loop: &mut mio::Poll, events: mio::Ready);
    fn register(&self, event_loop: &mut mio::Poll, token: mio::Token) -> Result<(), Error>;
    fn deregister(&self, event_loop: &mut mio::Poll) -> Result<(), Error>;
    fn process_rules(
        &mut self,
        event_loop: &mut mio::Poll,
        to_parent: &mut FnMut(MiChatCommand),
    ) -> Result<(), Error>;
}

impl MiChat {
    fn new() -> MiChat {
        MiChat {
            connections: Slab::new(),
        }
    }

    fn listen(&mut self, event_loop: &mut mio::Poll, listener: TcpListener) -> Result<(), Error> {
        let token = {
            let e = self.connections.vacant_entry();
            let token = mio::Token(e.key());
            let l = EventHandler::Listener(Listener::new(listener, token));
            e.insert(l);
            token
        };
        &self.connections[token.into()].register(event_loop, token)?;
        Ok(())
    }

    fn process_action(
        &mut self,
        src: usize,
        msg: MiChatCommand,
        event_loop: &mut mio::Poll,
    ) -> Result<(), Error> {
        trace!("{:p}; from {:?}; got {:?}", self, src, msg);
        match msg {
            MiChatCommand::Broadcast(s) => self.process_broadcast(s)?,
            MiChatCommand::NewConnection(socket) => {
                self.process_new_connection(event_loop, socket)?
            }
            MiChatCommand::CloseConnection => self.process_close(src, event_loop)?,
        }
        Ok(())
    }

    fn process_broadcast(&mut self, s: String) -> Result<(), Error> {
        for c in self.client_connections_mut() {
            c.enqueue(&s);
        }
        Ok(())
    }

    fn process_new_connection(
        &mut self,
        event_loop: &mut mio::Poll,
        socket: mio::tcp::TcpStream,
    ) -> Result<(), Error> {
        let token = {
            let e = self.connections.vacant_entry();
            let token = mio::Token(e.key());
            let l = EventHandler::Conn(Connection::new(socket, token));
            e.insert(l);
            token
        };
        &self.connections[token.into()].register(event_loop, token)?;
        Ok(())
    }

    fn process_close(&mut self, src: usize, event_loop: &mut mio::Poll) -> Result<(), Error> {
        debug!("Processing close: {:?}", src);
        let conn = self.connections.remove(src);
        conn.deregister(event_loop)?;
        Ok(())
    }

    fn client_connections_mut<'a>(&'a mut self) -> impl Iterator<Item = &'a mut Connection> + 'a {
        self.connections.iter_mut().filter_map(|(_, it)| match it {
            &mut EventHandler::Conn(ref mut c) => Some(c),
            _ => None,
        })
    }
}

#[derive(Debug)]
struct Connection {
    socket: TcpStream,
    sock_status: mio::Ready,
    token: mio::Token,
    read_buf: Vec<u8>,
    read_eof: bool,
    write_buf: Vec<u8>,
}

impl RuleHandler for Connection {
    fn register(&self, event_loop: &mut mio::Poll, token: mio::Token) -> Result<(), Error> {
        event_loop
            .register(
                &self.socket,
                token,
                mio::Ready::readable(),
                mio::PollOpt::edge() | mio::PollOpt::oneshot(),
            ).context("event loop register")?;
        Ok(())
    }
    fn deregister(&self, event_loop: &mut mio::Poll) -> Result<(), Error> {
        event_loop.deregister(&self.socket)?;
        Ok(())
    }

    // Event updates arrive here
    fn handle_event(&mut self, _event_loop: &mut mio::Poll, events: mio::Ready) {
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
        event_loop: &mut mio::Poll,
        to_parent: &mut FnMut(MiChatCommand),
    ) -> Result<(), Error> {
        if self.sock_status.is_readable() {
            self.sock_status.remove(mio::Ready::readable());
            self.read()?;
        }

        self.process_buffer(to_parent);

        if self.sock_status.is_writable() {
            self.sock_status.remove(mio::Ready::writable());
            self.write()?;
        }

        if self.should_close() {
            to_parent(MiChatCommand::CloseConnection)
        } else {
            self.reregister(event_loop)?;
        }

        Ok(())
    }
}

impl Connection {
    fn new(socket: TcpStream, token: mio::Token) -> Connection {
        Connection {
            socket: socket,
            sock_status: mio::Ready::empty(),
            token: token,
            read_buf: Vec::with_capacity(1024),
            write_buf: Vec::new(),
            read_eof: false,
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

    fn read(&mut self) -> Result<(), Error> {
        let mut abuf = vec![0; 1024];
        match self.socket.read(&mut abuf).context("Reading socket")? {
            0 => {
                info!("{:?}: EOF!", self.socket.peer_addr());
                self.read_eof = true;
                Ok(())
            }
            n => {
                info!("{:?}: Read {}bytes", self.socket.peer_addr(), n);
                self.read_buf.extend(&abuf[0..n]);
                Ok(())
            }
        }
    }

    fn write(&mut self) -> Result<(), Error> {
        let n = self
            .socket
            .write(&mut self.write_buf)
            .context("writing buffer")?;
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
        Ok(())
    }

    fn should_close(&self) -> bool {
        self.read_eof && self.write_buf.is_empty()
    }

    fn reregister(&mut self, event_loop: &mut mio::Poll) -> Result<(), Error> {
        let mut flags = mio::Ready::readable();
        if !self.write_buf.is_empty() {
            flags.insert(mio::Ready::writable());
        }
        info!("Registering {:?} with {:?}", self, flags);

        event_loop
            .reregister(&self.socket, self.token, flags, mio::PollOpt::oneshot())
            .context("EventLoop#reregister")?;

        Ok(())
    }

    fn enqueue(&mut self, s: &str) {
        self.write_buf.extend(s.as_bytes());
        self.write_buf.push('\n' as u8);
    }
}

#[derive(Debug)]
struct Listener {
    listener: TcpListener,
    sock_status: mio::Ready,
    token: mio::Token,
}

impl Listener {
    fn new(listener: TcpListener, token: mio::Token) -> Listener {
        Listener {
            listener: listener,
            sock_status: mio::Ready::empty(),
            token: token,
        }
    }
    fn reregister(&self, event_loop: &mut mio::Poll, token: mio::Token) -> Result<(), Error> {
        event_loop
            .reregister(
                &self.listener,
                token,
                mio::Ready::readable(),
                mio::PollOpt::edge() | mio::PollOpt::oneshot(),
            ).context("Register listener")?;
        Ok(())
    }
}

impl RuleHandler for Listener {
    fn register(&self, event_loop: &mut mio::Poll, token: mio::Token) -> Result<(), Error> {
        event_loop
            .register(
                &self.listener,
                token,
                mio::Ready::readable(),
                mio::PollOpt::edge() | mio::PollOpt::oneshot(),
            ).context("Register listener")?;
        Ok(())
    }
    fn deregister(&self, event_loop: &mut mio::Poll) -> Result<(), Error> {
        event_loop.deregister(&self.listener)?;
        Ok(())
    }

    fn handle_event(&mut self, _event_loop: &mut mio::Poll, events: mio::Ready) {
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
        event_loop: &mut mio::Poll,
        to_parent: &mut FnMut(MiChatCommand),
    ) -> Result<(), Error> {
        if self.sock_status.is_readable() {
            info!("the listener socket is ready to accept a connection");
            self.sock_status.remove(mio::Ready::readable());
            let (socket, _client) = self.listener.accept().context("Accept listening socket")?;
            let cmd = MiChatCommand::NewConnection(socket);
            to_parent(cmd);

            self.reregister(event_loop, self.token)?;
        }
        Ok(())
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

impl MiChat {
    fn ready(&mut self, event_loop: &mut mio::Poll, token: mio::Token, events: mio::Ready) {
        info!("{:?}: {:?}", token, events);
        self.connections[token.into()].handle_event(event_loop, events);
    }

    fn process(&mut self, event_loop: &mut mio::Poll) -> Result<(), Error> {
        let mut parent_actions = VecDeque::new();
        let mut failed = Vec::new();
        loop {
            let mut something_done = false;
            for (idx, conn) in self.connections.iter_mut() {
                if let Err(e) = conn.process_rules(event_loop, &mut |action| {
                    parent_actions.push_back((idx, action))
                }) {
                    failed.push(idx);
                    error!("Error on connection {:?}; Dropping: {:?}", idx, e);
                }
            }
            for idx in failed.drain(..) {
                debug!("Remove failed; {:?}", idx);
                let conn = self.connections.remove(idx);
                conn.deregister(event_loop)?;
            }
            for (src, action) in parent_actions.drain(..) {
                something_done = true;
                if let Err(e) = self.process_action(src, action, event_loop) {
                    failed.push(src);
                    error!("Error on action for {:?}; Dropping: {:?}", src, e);
                }
            }
            // If we didn't do anything, we're done.
            if !something_done {
                break;
            }
        }

        Ok(())
    }

    fn run(&mut self, event_loop: &mut mio::Poll) -> Result<(), Error> {
        loop {
            self.run_once(event_loop)?;
        }
    }
    fn run_once(&mut self, event_loop: &mut mio::Poll) -> Result<(), Error> {
        let mut events = mio::Events::with_capacity(1024);
        event_loop.poll(&mut events, None)?;
        for ev in &events {
            self.ready(event_loop, ev.token(), ev.readiness())
        }

        self.process(event_loop)
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

    let mut event_loop = mio::Poll::new().expect("Create event loop");
    let mut service = MiChat::new();

    service
        .listen(&mut event_loop, listener)
        .expect("Starting listener");
    info!("running michat listener at: {:?}", address);
    service.run(&mut event_loop).expect("run");
}
