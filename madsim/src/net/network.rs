use crate::{rand::*, time::TimeHandle};
use bytes::Bytes;
use futures::channel::oneshot;
use log::*;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    ops::Range,
    sync::{Arc, Mutex},
    time::Duration,
};

/// A simulated network.
pub(crate) struct Network {
    rand: RandomHandle,
    time: TimeHandle,
    config: Config,
    stat: Stat,
    endpoints: HashMap<SocketAddr, Arc<Mutex<Endpoint>>>,
    clogged: HashSet<SocketAddr>,
}

#[derive(Debug)]
pub struct Config {
    pub packet_loss_rate: f64,
    pub send_latency: Range<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            packet_loss_rate: 0.0,
            send_latency: Duration::from_millis(1)..Duration::from_millis(10),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stat {
    pub msg_count: u64,
}

impl Network {
    pub fn new(rand: RandomHandle, time: TimeHandle) -> Self {
        Self {
            rand,
            time,
            config: Config::default(),
            stat: Stat::default(),
            endpoints: HashMap::new(),
            clogged: HashSet::new(),
        }
    }

    pub fn update_config(&mut self, f: impl FnOnce(&mut Config)) {
        f(&mut self.config);
    }

    pub fn stat(&self) -> &Stat {
        &self.stat
    }

    pub fn insert(&mut self, target: SocketAddr) {
        if self.endpoints.contains_key(&target) {
            return;
        }
        trace!("insert: {}", target);
        self.endpoints.insert(target, Default::default());
    }

    pub fn remove(&mut self, target: &SocketAddr) {
        trace!("remove: {}", target);
        self.endpoints.remove(target);
        self.clogged.remove(target);
    }

    pub fn clog(&mut self, target: SocketAddr) {
        assert!(self.endpoints.contains_key(&target));
        trace!("clog: {}", target);
        self.clogged.insert(target);
    }

    pub fn unclog(&mut self, target: SocketAddr) {
        assert!(self.endpoints.contains_key(&target));
        trace!("unclog: {}", target);
        self.clogged.remove(&target);
    }

    pub fn send(&mut self, src: SocketAddr, dst: SocketAddr, tag: u64, data: &[u8]) {
        trace!("send: {} -> {}, tag={}, len={}", src, dst, tag, data.len());
        assert!(self.endpoints.contains_key(&src));
        if !self.endpoints.contains_key(&dst)
            || self.clogged.contains(&src)
            || self.clogged.contains(&dst)
            || self.rand.should_fault(self.config.packet_loss_rate)
        {
            trace!("drop");
            return;
        }
        let ep = self.endpoints[&dst].clone();
        let msg = Message {
            tag,
            data: Bytes::copy_from_slice(data),
            from: src,
        };
        let latency = self.rand.gen_range(self.config.send_latency.clone());
        trace!("delay: {:?}", latency);
        self.time.add_timer(self.time.now() + latency, move || {
            ep.lock().unwrap().send(msg);
        });
        self.stat.msg_count += 1;
    }

    pub fn recv(&mut self, dst: SocketAddr, tag: u64) -> oneshot::Receiver<Message> {
        self.endpoints[&dst].lock().unwrap().recv(tag)
    }
}

pub struct Message {
    pub tag: u64,
    pub data: Bytes,
    pub from: SocketAddr,
}

#[derive(Default)]
struct Endpoint {
    registered: Vec<(u64, oneshot::Sender<Message>)>,
    msgs: Vec<Message>,
}

impl Endpoint {
    fn send(&mut self, msg: Message) {
        if let Some(idx) = self.registered.iter().position(|(tag, _)| *tag == msg.tag) {
            let (_, sender) = self.registered.swap_remove(idx);
            let _ = sender.send(msg);
        } else {
            self.msgs.push(msg);
        }
    }

    fn recv(&mut self, tag: u64) -> oneshot::Receiver<Message> {
        let (tx, rx) = oneshot::channel();
        if let Some(idx) = self.msgs.iter().position(|msg| tag == msg.tag) {
            let msg = self.msgs.swap_remove(idx);
            tx.send(msg).ok().unwrap();
        } else {
            self.registered.push((tag, tx));
        }
        rx
    }
}
