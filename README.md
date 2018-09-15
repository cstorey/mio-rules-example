Rules-based Mio chat example.
============================

One thing that you notice after spending most of your time looking at the insides of a program, is that it's very easy to get bogged down in implementation detail, and end up with rather an optimistic view of how well the world outside of your application works. This is an especially common theme in the distributed systems field, and Peter Deutsch's infamous [Eight Fallacies of Distributed Computing](https://blogs.oracle.com/jag/resource/Fallacies.html) are a good example of this. 

I recently read a [blog post from Adrian Colyer](http://blog.acolyer.org/2016/01/19/dcft/) on the paper "[Experience with Rules-Based Programming for Distributed, Concurrent, Fault-Tolerant Code](http://web.stanford.edu/~ouster/cgi-bin/papers/rules-atc15)". I've been fiddling with the idea of implementing distributed systems in [Rust](http://rust-lang.org/) recently, so I figured I'd spend some seeing how well they fit together.

A principal contribution of the paper itself, is the description of an implementation pattern where rather than having input events from the outside world drive a traditional state machine directly, we split the event handling into updating our understanding of the world, and then acting based on the updated state.

In the paper, they give the example of the Hadoop Job tracker, and note that it's implemented using a fairly traditional state machine mechanism, where nodes in the graph are represented by an java enumeration type, and handlers are attached to state transition arcs. In total, this takes up about weighs in at a rather less efficient 2250 lines of Java to implement. They find that the handlers often represent quite similar actions, and so those handlers can contain a lot of duplicated code. They provide a link to a re-implementation of the model in python in approximately 120 lines of code, and demonstrate that is behaviorally equivalent to the original. If nothing else, it makes it far easier to get a feel for the shape of the code just by reading it.

So, in the spirit of reductive examples everywhere, our example comes in the form of a chat server. Where the paper operates on top of an existing RPC abstraction, we apply it directly on top of the socket interface provided by [mio](https://crates.io/crates/mio/). The bulk of our processing happens in the [`Handler#ready`](https://github.com/cstorey/mio-rules-example/blob/25be0cf04c66a526eb6008dfe587d56120d07e51/src/main.rs#L293-L313) method of our handler. So in the terminology of the paper, we have two tasks, namely listening for new connections, and handling established connections.

Because our tasks operate mostly at the transport layer, our events are essentially socket readiness notifications, and the bulk of the actions include such exciting things as reading/writing from sockets. For example, the [event handler for the listening socket](https://github.com/cstorey/mio-rules-example/blob/25be0cf04c66a526eb6008dfe587d56120d07e51/src/main.rs#L222-L227) looks like:


```rust
    fn handle_event(&mut self, _event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet) {
        assert!(events.is_readable());
        self.sock_status.insert(events);
        info!("Listener::handle_event: {:?}; this time: {:?}; now: {:?}",
                self.listener.local_addr(), events, self.sock_status);
    }
```

Then [for each connection](https://github.com/cstorey/mio-rules-example/blob/25be0cf04c66a526eb6008dfe587d56120d07e51/src/main.rs#L222-L227) is remarkably similar:

```rust
    fn handle_event(&mut self, _event_loop: &mut mio::EventLoop<MiChat>, events: mio::EventSet) {
        assert!(events.is_readable());
        self.sock_status.insert(events);
        info!("Listener::handle_event: {:?}; this time: {:?}; now: {:?}",
                self.listener.local_addr(), events, self.sock_status);
    }
```

And the bulk of the activity happens in the [`Listener#process_rules`](https://github.com/cstorey/mio-rules-example/blob/25be0cf04c66a526eb6008dfe587d56120d07e51/src/main.rs#L229) method:

```rust
    fn process_rules(&mut self, event_loop: &mut mio::EventLoop<MiChat>,
            to_parent: &mut VecDeque<MiChatCommand>) {
        info!("the listener socket is ready to accept a connection");
        match self.listener.accept() {
            Ok(Some(socket)) => {
                let cmd = MiChatCommand::NewConnection(socket);
                to_parent.push_back(cmd);
            }
	    // ...
        }
    }
```

So, when our listening socket is readable (ie: has a client attempting to connect), we `accept(2)` the connection, and notify the service of the connection, by passing a `MiChatCommand::NewConnection` into the queue of outstanding actions to be processed.  The [`Connection#process_rules`](https://github.com/cstorey/mio-rules-example/blob/25be0cf04c66a526eb6008dfe587d56120d07e51/src/main.rs#L103) method is a little more complex, as it handles both reading, writing to the socket, and invokes [`Connection#process_buffer`](https://github.com/cstorey/mio-rules-example/blob/25be0cf04c66a526eb6008dfe587d56120d07e51/src/main.rs#L122) to actually parse the input into "frames" (lines in this case).

```rust
    fn process_rules(&mut self, event_loop: &mut mio::EventLoop<MiChat>,
        to_parent: &mut VecDeque<MiChatCommand>) {
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
```

Whenever we see a full line of input from the client, we pass a `MiChatCommand::Broadcast`, we pass that to [`MiChat#process_action`](https://github.com/cstorey/mio-rules-example/blob/25be0cf04c66a526eb6008dfe587d56120d07e51/src/main.rs#L40), which will in turn enqueue the message on each connection for output.

```rust
    fn process_action(&mut self, msg: MiChatCommand, event_loop: &mut mio::EventLoop<MiChat>) {
        trace!("{:p}; got {:?}", self, msg);
        match msg {
            MiChatCommand::Broadcast(s) => {
                for c in self.connections.iter_mut() {
                    if let &mut EventHandler::Conn(ref mut c) = c {
                        c.enqueue(&s);
                    }
                }
            },
            // ...
        }
    }

```

The rules processing in the `Handler#ready` implementation then halts once everything we are concerned about has quiesced; in this case, when no more actions are generated by our rules. 

Personally, I do like this approach; it seems easier to end up with code that is well factor, and can potentially encourage a reasonable layering of concerns. For example, it is fairly easy to seperate the frame handling code from that which deals with the underlying socket manipulation by thinking of it as a stack of adaptors, each of which bridging a few layers from the ISO layer model, which ultimately communicates with your application model.

The full code can be found in the [mio-rules-example](https://github.com/cstorey/mio-rules-example).
