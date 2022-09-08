//! Asynchronous network endpoint and a controlled network simulator.
//!
//! # Examples
//!
//! ```
//! use msim::{runtime::Runtime, net::Endpoint};
//! use std::sync::Arc;
//! use std::net::SocketAddr;
//!
//! let runtime = Runtime::new();
//! let addr1 = "10.0.0.1:1".parse::<SocketAddr>().unwrap();
//! let addr2 = "10.0.0.2:1".parse::<SocketAddr>().unwrap();
//! let node1 = runtime.create_node().ip(addr1.ip()).build();
//! let node2 = runtime.create_node().ip(addr2.ip()).build();
//! let barrier = Arc::new(tokio::sync::Barrier::new(2));
//! let barrier_ = barrier.clone();
//!
//! node1.spawn(async move {
//!     let net = Endpoint::bind(addr1).await.unwrap();
//!     barrier_.wait().await;  // make sure addr2 has bound
//!
//!     net.send_to(addr2, 1, &[1]).await.unwrap();
//! });
//!
//! let f = node2.spawn(async move {
//!     let net = Endpoint::bind(addr2).await.unwrap();
//!     barrier.wait().await;
//!
//!     let mut buf = vec![0; 0x10];
//!     let (len, from) = net.recv_from(1, &mut buf).await.unwrap();
//!     assert_eq!(from, addr1);
//!     assert_eq!(&buf[..len], &[1]);
//! });
//!
//! runtime.block_on(f);
//! ```

use std::{
    collections::{hash_map::Entry, HashMap},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    sync::{Arc, Mutex},
    task::Context,
};
use tracing::*;

pub use self::network::{Config, Stat};
use self::network::{Network, Payload};
use crate::{
    define_bypass, define_sys_interceptor, plugin,
    rand::{GlobalRng, Rng},
    task::NodeId,
    time::{Duration, TimeHandle},
};

/// network module
#[allow(missing_docs)]
pub mod network;
#[cfg(feature = "rpc")]
#[cfg_attr(docsrs, doc(cfg(feature = "rpc")))]
pub mod rpc;

/// Network simulator.
#[cfg_attr(docsrs, doc(cfg(msim)))]
pub struct NetSim {
    network: Mutex<Network>,
    host_state: Mutex<HostNetworkState>,
    rand: GlobalRng,
    time: TimeHandle,
    next_port_map: Mutex<HashMap<NodeId, u32>>,
}

#[derive(Debug)]
struct FileDes(libc::c_int);

impl Drop for FileDes {
    fn drop(&mut self) {
        unsafe {
            assert_eq!(bypass_close(self.0), 0);
        }
    }
}

#[derive(Debug)]
struct SocketState {
    ty: libc::c_int,
    _placeholder_file: FileDes,
    endpoint: Option<Arc<Endpoint>>,
}

#[derive(Default)]
struct HostNetworkState {
    sockets: HashMap<(NodeId, libc::c_int), SocketState>,
}

impl HostNetworkState {
    fn add_socket(fd: libc::c_int, socket: SocketState) {
        let net = plugin::simulator::<NetSim>();
        let node_id = plugin::node();
        let mut host_state = net.host_state.lock().unwrap();
        trace!("registering socket {}.{} -> {:?}", node_id, fd, socket);

        assert!(
            host_state.sockets.insert((node_id, fd), socket).is_none(),
            "duplicate socket"
        );
    }

    fn bind_socket(fd: libc::c_int, addr: SocketAddr) -> libc::c_int {
        let net = plugin::simulator::<NetSim>();
        let node_id = plugin::node();
        let mut host_state = net.host_state.lock().unwrap();

        trace!("binding socket {}.{} -> {:?}", node_id, fd, addr);

        let socket = host_state.sockets.get_mut(&(node_id, fd)).unwrap();
        assert!(socket.endpoint.is_none(), "socket already bound");
        socket.endpoint = Some(Arc::new(Endpoint::bind_sync(addr).unwrap()));
        0
    }

    fn close_socket(fd: libc::c_int) -> bool {
        let net = plugin::simulator::<NetSim>();
        let node_id = plugin::node();
        let mut host_state = net.host_state.lock().unwrap();

        let res = host_state.sockets.remove(&(node_id, fd)).is_some();
        if res {
            trace!("closing socket {}.{}", node_id, fd);
        }
        res
    }

    fn get_socket_addr(fd: libc::c_int) -> Option<SocketAddr> {
        let net = plugin::simulator::<NetSim>();
        let node_id = plugin::node();
        let host_state = net.host_state.lock().unwrap();

        let socket = host_state.sockets.get(&(node_id, fd))?;
        socket.endpoint.as_ref().map(|ep| ep.local_addr().unwrap())
    }

    fn with_socket<T>(fd: libc::c_int, cb: impl Fn(&mut SocketState) -> T) -> io::Result<T> {
        let net = plugin::simulator::<NetSim>();
        let node_id = plugin::node();
        let mut host_state = net.host_state.lock().unwrap();
        let socket = host_state
            .sockets
            .get_mut(&(node_id, fd))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no such socket"))?;
        Ok(cb(socket))
    }
}

/// Get the Endpoint of a bound socket.
pub fn get_endpoint_from_socket(fd: libc::c_int) -> io::Result<Arc<Endpoint>> {
    HostNetworkState::with_socket(fd, |socket| socket.endpoint.as_ref().map(|ep| ep.clone()))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket has not been bound"))
}

define_bypass!(bypass_close, fn close(fd: libc::c_int) -> libc::c_int);

define_sys_interceptor!(
    fn close(fd: libc::c_int) -> libc::c_int {
        if HostNetworkState::close_socket(fd) {
            return 0;
        }
        trace!("forwarding close({}) to libc", fd);
        NEXT_DL_SYM(fd)
    }
);

#[cfg(target_os = "macos")]
unsafe fn set_errno(err: libc::c_int) {
    *libc::__error() = err;
}

#[cfg(target_os = "linux")]
unsafe fn set_errno(err: libc::c_int) {
    *libc::__errno_location() = err;
}

define_sys_interceptor!(
    fn bind(
        sock_fd: libc::c_int,
        sock_addr: *const libc::sockaddr,
        addr_len: libc::socklen_t,
    ) -> libc::c_int {
        let socket_addr = socket2::SockAddr::init(|storage, len| {
            std::ptr::copy_nonoverlapping(
                sock_addr as *const u8,
                storage as *mut u8,
                std::cmp::min(*len, addr_len) as usize,
            );
            Ok(())
        })
        .unwrap()
        .1
        .as_socket()
        .unwrap();

        if socket_addr.is_ipv6() {
            warn!("ipv6 not supported in simulator");
            set_errno(libc::EADDRNOTAVAIL);
            return -1;
        }

        HostNetworkState::bind_socket(sock_fd, socket_addr)
    }
);

define_sys_interceptor!(
    fn connect(
        socket: libc::c_int,
        address: *const libc::sockaddr,
        len: libc::socklen_t,
    ) -> libc::c_int {
        todo!();
    }
);

define_sys_interceptor!(
    fn socket(domain: libc::c_int, ty: libc::c_int, protocol: libc::c_int) -> libc::c_int {
        assert!(
            domain == libc::AF_INET || domain == libc::AF_INET6,
            "only ip4 sockets are currently supported"
        );

        if protocol != 0 {
            warn!("socket(): non-zero protocol ignored - application intent may not be respected");
        }

        // Allocate a new file descriptor - it will never be used. We don't want to allocate fds
        // ourselves because the program may allocate an fd from some other means (like open())
        // which could collide with any descriptor we choose.
        let fd = libc::dup(0);

        let socket = SocketState {
            ty,
            _placeholder_file: FileDes(fd),
            endpoint: None,
        };

        HostNetworkState::add_socket(fd, socket);

        fd
    }
);

define_sys_interceptor!(
    fn getsockname(
        socket: libc::c_int,
        address: *mut libc::sockaddr,
        address_len: *mut libc::socklen_t,
    ) -> libc::c_int {
        trace!("getsockname({})", socket);
        // getsockname() on an un-bound socket does not actually return an error - instead it has
        // unspecified behavior. But we can just panic, since doing this would be a bug anyway.
        let addr: socket2::SockAddr = HostNetworkState::get_socket_addr(socket)
            .expect("getsockname() on un-bound socket")
            .into();

        let len = std::cmp::min(*address_len as usize, addr.len() as usize);

        std::ptr::copy_nonoverlapping(addr.as_ptr() as *const u8, address as *mut u8, len);

        let address_len = &mut *address_len;
        *address_len = addr.len();

        0
    }
);

define_sys_interceptor!(
    fn setsockopt(
        socket: libc::c_int,
        level: libc::c_int,
        name: libc::c_int,
        value: *const libc::c_void,
        option_len: libc::socklen_t,
    ) -> libc::c_int {
        trace!("setsockopt({}, {}, {})", socket, level, name);
        match (level, name) {
            (libc::IPPROTO_IPV6, _) => unimplemented!("ipv6 not supported"),

            // called by rust std::net::TcpListener::bind
            // No need to actually emulate SO_REUSEADDR behavior (for now).
            (libc::SOL_SOCKET, libc::SO_REUSEADDR) => 0,

            // call by std::net::TcpStream::set_ttl
            (libc::IPPROTO_IP, libc::IP_TTL) => 0,

            // called by rust std::net::UdpSocket::bind
            #[cfg(target_os = "macos")]
            (libc::SOL_SOCKET, libc::SO_NOSIGPIPE) => 0,

            // Call by quinn, no need to emulate (for now)
            (libc::IPPROTO_IP, libc::IP_RECVTOS) => 0,
            (libc::IPPROTO_IP, libc::IP_PKTINFO) => 0,

            // The simulator never fragments or anything like that, so there is no need to simulate
            // this option.
            #[cfg(target_os = "linux")]
            (libc::IPPROTO_IP, libc::IP_MTU_DISCOVER) => 0,

            // simulator doesn't simulate GRO/GSO
            #[cfg(target_os = "linux")]
            (libc::SOL_UDP, libc::UDP_GRO) => -1,
            #[cfg(target_os = "linux")]
            (libc::SOL_UDP, libc::UDP_SEGMENT) => -1,

            _ => {
                warn!("unhandled socket option {} {}", level, name);
                0
            }
        }
    }
);

define_sys_interceptor!(
    fn send(
        sockfd: libc::c_int,
        buf: *const libc::c_void,
        len: libc::size_t,
        flags: libc::c_int,
    ) -> libc::ssize_t {
        unimplemented!("simulator error: send() should have been handled by tokio");
    }
);

define_sys_interceptor!(
    fn sendto(
        sockfd: libc::c_int,
        buf: *const libc::c_void,
        len: libc::size_t,
        flags: libc::c_int,
        dest_addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> libc::ssize_t {
        unimplemented!("simulator error: sendto() should have been handled by tokio");
    }
);

enum UDPMessage {
    Payload(Vec<u8>),
}

impl UDPMessage {
    fn payload(v: Vec<u8>) -> Box<UDPMessage> {
        Box::new(UDPMessage::Payload(v))
    }

    fn into_payload(self) -> Vec<u8> {
        match self {
            Self::Payload(v) => v,
        }
    }
}

unsafe fn msg_hdr_to_socket(msg: &libc::msghdr) -> SocketAddr {
    socket2::SockAddr::init(|storage, len| {
        std::ptr::copy_nonoverlapping(
            msg.msg_name as *const u8,
            storage as *mut u8,
            std::cmp::min(*len, msg.msg_namelen) as usize,
        );
        Ok(())
    })
    .unwrap()
    .1
    .as_socket()
    .unwrap()
}

unsafe fn send_impl(
    socket: &mut SocketState,
    dst_addr: &SocketAddr,
    flags: libc::c_int,
    iov: &libc::iovec,
) -> libc::ssize_t {
    assert_eq!(
        socket.ty,
        libc::SOCK_DGRAM,
        "only UDP is supported in sendmsg/sendmmsg"
    );

    if flags != 0 {
        warn!("unsupported flags to sendmsg/sendmmsg: {:x}", flags);
    }

    // TODO: we are not currently emulating control msgs, such as IP_PKTINFO -
    // QUIC relies on this in situations where a socket is bound to 0.0.0.0 and there are multiple
    // interfaces/ip addresses. However, simulated nodes don't have multiple IPs, so this doesn't
    // affect us.
    let slice = std::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len);
    let msg = UDPMessage::payload(slice.into());

    // If we need to handle sending from unconnected sockets, we can make an ephemeral
    // endpoint.
    let ep = socket
        .endpoint
        .as_ref()
        .expect("sendmsg on unconnected sockets not supported");

    ep.send_to_raw_sync(*dst_addr, dst_addr.port().into(), msg);

    slice.len() as libc::ssize_t
}

define_sys_interceptor!(
    fn sendmsg(sockfd: libc::c_int, msg: *const libc::msghdr, flags: libc::c_int) -> libc::ssize_t {
        HostNetworkState::with_socket(sockfd, |socket| {
            let msg = &*msg;
            let dst_addr = msg_hdr_to_socket(msg);

            assert_eq!(msg.msg_iovlen, 1, "scatter/gather unsupported");

            let iov = &*msg.msg_iov;

            send_impl(socket, &dst_addr, flags, iov)
        })
        .unwrap_or_else(|e| {
            trace!("error: {}", e);
            set_errno(libc::EADDRNOTAVAIL);
            -1
        })
    }
);

#[cfg(target_os = "linux")]
define_sys_interceptor!(
    fn sendmmsg(
        sockfd: libc::c_int,
        msgvec: *mut libc::mmsghdr,
        vlen: libc::c_uint,
        flags: libc::c_int,
    ) -> libc::c_int {
        HostNetworkState::with_socket(sockfd, |socket| {
            let msgvec = &mut *msgvec;
            let msgs = std::slice::from_raw_parts(msgvec.msg_hdr, msgvec.msg_len);

            for msg in msgs {
                let dst_addr = msg_hdr_to_socket(msg.msg_hdr);
                assert_eq!(msg.msg_hdr.msg_iovlen, 1, "scatter/gather unsupported");
                let iov = &*msg.msg_hdr.msg_iov;

                msg.msg_len = send_impl(socket, &dst_addr, flags, iov);
            }
            msgs.len()
        })
        .unwrap_or_else(|e| {
            trace!("socket not found: {}", e);
            set_errno(libc::EADDRNOTAVAIL);
            -1
        })
    }
);

type CResult<T> = Result<T, (T, libc::c_int)>;

define_sys_interceptor!(
    fn recvmsg(sockfd: libc::c_int, msg: *mut libc::msghdr, flags: libc::c_int) -> libc::ssize_t {
        HostNetworkState::with_socket(sockfd, |socket| -> CResult<libc::ssize_t> {
            assert_eq!(
                socket.ty,
                libc::SOCK_DGRAM,
                "only UDP is supported in recvmsg/recvmmsg"
            );

            if flags != 0 {
                warn!("unsupported flags to sendmsg/sendmmsg: {:x}", flags);
            }

            // i'm not exactly clear what errno should be returned if you call recvmsg() without
            // bind(), so just assert. Working code won't trigger this.
            let ep = socket
                .endpoint
                .as_ref()
                .expect("recvmsg on un-bound socket");

            let udp_tag = ep.udp_tag().expect("recvmsg on un-bound socket");

            let (payload, from) =
                ep.recv_from_raw_sync(udp_tag)
                    .map_err(|err| match err.kind() {
                        io::ErrorKind::WouldBlock => (-1, libc::EAGAIN),
                        _ => todo!("unhandled error case"),
                    })?;

            let msg = &mut *msg;

            if !msg.msg_name.is_null() {
                let from: socket2::SockAddr = from.into();
                std::ptr::copy_nonoverlapping(
                    from.as_ptr() as *const u8,
                    msg.msg_name as *mut u8,
                    from.len() as usize,
                );
            }

            let payload = payload
                .downcast::<UDPMessage>()
                .expect("message was not UDPMessage")
                .into_payload();

            assert_eq!(msg.msg_iovlen, 1, "scatter/gather unsupported");

            let iov = &*msg.msg_iov;
            let copy_len = std::cmp::min(iov.iov_len, payload.len());
            if copy_len < payload.len() {
                msg.msg_flags |= libc::MSG_TRUNC;
            }
            std::ptr::copy_nonoverlapping(
                payload.as_ptr() as *const u8,
                iov.iov_base as *mut u8,
                copy_len,
            );

            // TODO: control message
            Ok(copy_len as libc::ssize_t)
        })
        .unwrap_or_else(|e| {
            trace!("socket not found: {}", e);
            // could also be EBADF, probably not worth trying to emulate perfectly.
            CResult::Err((-1, libc::ENOTSOCK))
        })
        .unwrap_or_else(|(ret, err)| {
            trace!("error status: {} {}", ret, err);
            set_errno(err);
            ret
        })
    }
);

#[cfg(target_os = "linux")]
define_sys_interceptor!(
    fn recvmmsg(
        sockfd: libc::c_int,
        msgvec: *mut libc::mmsghdr,
        vlen: libc::c_uint,
        flags: libc::c_int,
        timeout: *mut libc::timespec,
    ) -> libc::c_int {
        todo!();
    }
);

impl plugin::Simulator for NetSim {
    fn new(rand: &GlobalRng, time: &TimeHandle, config: &crate::Config) -> Self {
        NetSim {
            network: Mutex::new(Network::new(rand.clone(), time.clone(), config.net.clone())),
            rand: rand.clone(),
            time: time.clone(),
            host_state: Default::default(),
            next_port_map: Mutex::new(HashMap::new()),
        }
    }

    fn create_node(&self, id: NodeId) {
        let mut network = self.network.lock().unwrap();
        network.insert_node(id);
    }

    fn reset_node(&self, id: NodeId) {
        self.reset_node(id);
    }
}

impl NetSim {
    /// Get the statistics.
    pub fn stat(&self) -> Stat {
        self.network.lock().unwrap().stat().clone()
    }

    /// Update network configurations.
    pub fn update_config(&self, f: impl FnOnce(&mut Config)) {
        let mut network = self.network.lock().unwrap();
        network.update_config(f);
    }

    /// Reset a node.
    ///
    /// All connections will be closed.
    pub fn reset_node(&self, id: NodeId) {
        let mut network = self.network.lock().unwrap();
        network.reset_node(id);
    }

    /// Set IP address of a node.
    pub fn set_ip(&self, node: NodeId, ip: IpAddr) {
        let mut network = self.network.lock().unwrap();
        network.set_ip(node, ip);
    }

    /// Get IP address of a node.
    pub fn get_ip(&self, node: NodeId) -> Option<IpAddr> {
        let network = self.network.lock().unwrap();
        network.get_ip(node)
    }

    /// Connect a node to the network.
    pub fn connect(&self, id: NodeId) {
        let mut network = self.network.lock().unwrap();
        network.unclog_node(id);
    }

    /// Disconnect a node from the network.
    pub fn disconnect(&self, id: NodeId) {
        let mut network = self.network.lock().unwrap();
        network.clog_node(id);
    }

    /// Connect a pair of nodes.
    pub fn connect2(&self, node1: NodeId, node2: NodeId) {
        let mut network = self.network.lock().unwrap();
        network.unclog_link(node1, node2);
        network.unclog_link(node2, node1);
    }

    /// Disconnect a pair of nodes.
    pub fn disconnect2(&self, node1: NodeId, node2: NodeId) {
        let mut network = self.network.lock().unwrap();
        network.clog_link(node1, node2);
        network.clog_link(node2, node1);
    }

    async fn rand_delay(&self) {
        let delay = Duration::from_micros(self.rand.with(|rng| rng.gen_range(0..5)));
        self.time.sleep(delay).await;
    }

    /// Get the next unused port number for this node.
    pub fn next_local_port(&self, node: NodeId) -> u32 {
        let mut map = self.next_port_map.lock().unwrap();
        match map.entry(node) {
            Entry::Occupied(mut cur) => {
                let cur = cur.get_mut();
                *cur = cur.wrapping_add(1);
                *cur
            }
            Entry::Vacant(e) => {
                // ports start at 1, 0 is used for new connections (see poll_accept_internal)
                e.insert(1);
                1
            }
        }
    }
}

/// An endpoint.
pub struct Endpoint {
    net: Arc<NetSim>,
    node: NodeId,
    addr: SocketAddr,
    peer: Option<SocketAddr>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        fmt.debug_struct("Endpoint")
            .field("node", &self.node)
            .field("addr", &self.addr)
            .field("peer", &self.peer)
            .finish()
    }
}

impl Endpoint {
    /// Bind synchronously (for UDP)
    pub fn bind_sync(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let net = plugin::simulator::<NetSim>();
        let node = plugin::node();
        let addr = addr.to_socket_addrs()?.next().unwrap();
        let addr = net.network.lock().unwrap().bind(node, addr)?;
        Ok(Endpoint {
            net,
            node,
            addr,
            peer: None,
        })
    }

    /// return the tag used to send to this endpooint, for udp connections only.
    /// (It is the same as the udp port number).
    /// port is 0 we panic
    pub fn udp_tag(&self) -> io::Result<u64> {
        let port = self.addr.port();
        if port == 0 {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "endpoint is not bound",
            ))
        } else {
            Ok(port as u64)
        }
    }

    /// Creates a [`Endpoint`] from the given address.
    pub async fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let net = plugin::simulator::<NetSim>();
        let node = plugin::node();
        let addr = addr.to_socket_addrs()?.next().unwrap();
        net.rand_delay().await;
        let addr = net.network.lock().unwrap().bind(node, addr)?;
        Ok(Endpoint {
            net,
            node,
            addr,
            peer: None,
        })
    }

    /// Connects this [`Endpoint`] to a remote address.
    pub async fn connect(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let net = plugin::simulator::<NetSim>();
        let node = plugin::node();
        let peer = addr.to_socket_addrs()?.next().unwrap();
        net.rand_delay().await;
        let addr = if peer.ip().is_loopback() {
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
        } else {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        };
        let addr = net.network.lock().unwrap().bind(node, addr)?;
        Ok(Endpoint {
            net,
            node,
            addr,
            peer: Some(peer),
        })
    }

    /// Allocate a new "port" number for this node. Ports are never reused.
    pub fn allocate_local_port(&self) -> u32 {
        self.net.next_local_port(self.node)
    }

    /// Returns the local socket address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr)
    }

    /// Returns the socket address of the remote peer this socket was connected to.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.peer
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "not connected"))
    }

    /// Sends data with tag on the socket to the given address.
    ///
    /// # Example
    /// ```
    /// use msim::{runtime::Runtime, net::Endpoint};
    ///
    /// Runtime::new().block_on(async {
    ///     let net = Endpoint::bind("127.0.0.1:0").await.unwrap();
    ///     net.send_to("127.0.0.1:4242", 0, &[0; 10]).await.expect("couldn't send data");
    /// });
    /// ```
    pub async fn send_to(&self, dst: impl ToSocketAddrs, tag: u64, buf: &[u8]) -> io::Result<()> {
        let dst = dst.to_socket_addrs()?.next().unwrap();
        self.send_to_raw(dst, tag, Box::new(Vec::from(buf))).await
    }

    /// Receives a single message with given tag on the socket.
    /// On success, returns the number of bytes read and the origin.
    ///
    /// # Example
    /// ```no_run
    /// use msim::{runtime::Runtime, net::Endpoint};
    ///
    /// Runtime::new().block_on(async {
    ///     let net = Endpoint::bind("127.0.0.1:0").await.unwrap();
    ///     let mut buf = [0; 10];
    ///     let (len, src) = net.recv_from(0, &mut buf).await.expect("couldn't receive data");
    /// });
    /// ```
    pub async fn recv_from(&self, tag: u64, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let (data, from) = self.recv_from_raw(tag).await?;
        // copy to buffer
        let data = data.downcast::<Vec<u8>>().expect("message is not data");
        let len = buf.len().min(data.len());
        buf[..len].copy_from_slice(&data[..len]);
        Ok((len, from))
    }

    /// Sends data on the socket to the remote address to which it is connected.
    pub async fn send(&self, tag: u64, buf: &[u8]) -> io::Result<()> {
        let peer = self.peer_addr()?;
        self.send_to(peer, tag, buf).await
    }

    /// Receives a single datagram message on the socket from the remote address to which it is connected.
    /// On success, returns the number of bytes read.
    pub async fn recv(&self, tag: u64, buf: &mut [u8]) -> io::Result<usize> {
        let peer = self.peer_addr()?;
        let (len, from) = self.recv_from(tag, buf).await?;
        assert_eq!(
            from, peer,
            "receive a message but not from the connected address"
        );
        Ok(len)
    }

    /// Sends a raw message.
    ///
    /// NOTE: Applications should not use this function!
    /// It is provided for use by other simulators.
    #[cfg_attr(docsrs, doc(cfg(msim)))]
    pub async fn send_to_raw(&self, dst: SocketAddr, tag: u64, data: Payload) -> io::Result<()> {
        self.send_to_raw_sync(dst, tag, data);
        self.net.rand_delay().await;
        Ok(())
    }

    /// Sends a raw message.
    ///
    /// NOTE: Applications should not use this function!
    /// It is provided for use by other simulators.
    #[cfg_attr(docsrs, doc(cfg(msim)))]
    pub fn send_to_raw_sync(&self, dst: SocketAddr, tag: u64, data: Payload) {
        trace!("send_to_raw {} -> {}, {:x}", self.addr, dst, tag);
        self.net
            .network
            .lock()
            .unwrap()
            .send(plugin::node(), self.addr, dst, tag, data);
    }

    /// Receives a raw message.
    ///
    /// NOTE: Applications should not use this function!
    /// It is provided for use by other simulators.
    #[cfg_attr(docsrs, doc(cfg(msim)))]
    pub async fn recv_from_raw(&self, tag: u64) -> io::Result<(Payload, SocketAddr)> {
        trace!("awaiting recv: {} tag={:x}", self.addr, tag);
        let recver = self
            .net
            .network
            .lock()
            .unwrap()
            .recv(plugin::node(), self.addr, tag);
        let msg = recver
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "network is down"))?;
        self.net.rand_delay().await;

        trace!("recv: {} <- {}, tag={:x}", self.addr, msg.from, msg.tag);
        Ok((msg.data, msg.from))
    }

    /// Receive a raw message, synchronously
    pub fn recv_from_raw_sync(&self, tag: u64) -> io::Result<(Payload, SocketAddr)> {
        let msg = self
            .net
            .network
            .lock()
            .unwrap()
            .recv_sync(plugin::node(), self.addr, tag)
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "recv call would blck"))?;

        trace!(
            "recv sync: {} <- {}, tag={:x}",
            self.addr,
            msg.from,
            msg.tag
        );
        Ok((msg.data, msg.from))
    }

    /// Sends a raw message. to the connected remote address.
    ///
    /// NOTE: Applications should not use this function!
    /// It is provided for use by other simulators.
    #[cfg_attr(docsrs, doc(cfg(msim)))]
    pub async fn send_raw(&self, tag: u64, data: Payload) -> io::Result<()> {
        let peer = self.peer_addr()?;
        self.send_to_raw(peer, tag, data).await
    }

    /// Receives a raw message from the connected remote address.
    ///
    /// NOTE: Applications should not use this function!
    /// It is provided for use by other simulators.
    #[cfg_attr(docsrs, doc(cfg(msim)))]
    pub async fn recv_raw(&self, tag: u64) -> io::Result<Payload> {
        let peer = self.peer_addr()?;
        let (msg, from) = self.recv_from_raw(tag).await?;
        assert_eq!(
            from, peer,
            "receive a message but not from the connected address"
        );
        Ok(msg)
    }

    /// Check if there is a message waiting that can be received without blocking.
    /// If not, schedule a wakeup using the context.
    pub fn recv_ready(&self, cx: &mut Context<'_>, tag: u64) -> io::Result<bool> {
        Ok(self
            .net
            .network
            .lock()
            .unwrap()
            .recv_ready(cx, plugin::node(), self.addr, tag))
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // avoid panic on panicking
        if let Ok(mut network) = self.net.network.lock() {
            network.close(self.node, self.addr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{plugin::simulator, runtime::Runtime, time::*};
    use tokio::sync::Barrier;

    #[test]
    fn send_recv() {
        let runtime = Runtime::new();
        let addr1 = "10.0.0.1:1".parse::<SocketAddr>().unwrap();
        let addr2 = "10.0.0.2:1".parse::<SocketAddr>().unwrap();
        let node1 = runtime.create_node().ip(addr1.ip()).build();
        let node2 = runtime.create_node().ip(addr2.ip()).build();
        let barrier = Arc::new(Barrier::new(2));

        let barrier_ = barrier.clone();
        node1.spawn(async move {
            let net = Endpoint::bind(addr1).await.unwrap();
            barrier_.wait().await;

            net.send_to(addr2, 1, &[1]).await.unwrap();

            sleep(Duration::from_secs(1)).await;
            net.send_to(addr2, 2, &[2]).await.unwrap();
        });

        let f = node2.spawn(async move {
            let net = Endpoint::bind(addr2).await.unwrap();
            barrier.wait().await;

            let mut buf = vec![0; 0x10];
            let (len, from) = net.recv_from(2, &mut buf).await.unwrap();
            assert_eq!(len, 1);
            assert_eq!(from, addr1);
            assert_eq!(buf[0], 2);

            let (len, from) = net.recv_from(1, &mut buf).await.unwrap();
            assert_eq!(len, 1);
            assert_eq!(from, addr1);
            assert_eq!(buf[0], 1);
        });

        runtime.block_on(f).unwrap();
    }

    #[test]
    fn receiver_drop() {
        let runtime = Runtime::new();
        let addr1 = "10.0.0.1:1".parse::<SocketAddr>().unwrap();
        let addr2 = "10.0.0.2:1".parse::<SocketAddr>().unwrap();
        let node1 = runtime.create_node().ip(addr1.ip()).build();
        let node2 = runtime.create_node().ip(addr2.ip()).build();
        let barrier = Arc::new(Barrier::new(2));

        let barrier_ = barrier.clone();
        node1.spawn(async move {
            let net = Endpoint::bind(addr1).await.unwrap();
            barrier_.wait().await;

            net.send_to(addr2, 1, &[1]).await.unwrap();
        });

        let f = node2.spawn(async move {
            let net = Endpoint::bind(addr2).await.unwrap();
            let mut buf = vec![0; 0x10];
            timeout(Duration::from_secs(1), net.recv_from(1, &mut buf))
                .await
                .err()
                .unwrap();
            // timeout and receiver dropped here
            barrier.wait().await;

            // receive again should success
            let (len, from) = net.recv_from(1, &mut buf).await.unwrap();
            assert_eq!(len, 1);
            assert_eq!(from, addr1);
        });

        runtime.block_on(f).unwrap();
    }

    #[test]
    fn reset() {
        let runtime = Runtime::new();
        let addr1 = "10.0.0.1:1".parse::<SocketAddr>().unwrap();
        let node1 = runtime.create_node().ip(addr1.ip()).build();

        let f = node1.spawn(async move {
            let net = Endpoint::bind(addr1).await.unwrap();
            let err = net.recv_from(1, &mut []).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
            // FIXME: should still error
            // let err = net.recv_from(1, &mut []).await.unwrap_err();
            // assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        });

        runtime.block_on(async move {
            sleep(Duration::from_secs(1)).await;
            simulator::<NetSim>().reset_node(node1.id());
            f.await.unwrap();
        });
    }

    #[test]
    fn bind() {
        let runtime = Runtime::new();
        let ip = "10.0.0.1".parse::<IpAddr>().unwrap();
        let node = runtime.create_node().ip(ip).build();

        let f = node.spawn(async move {
            // unspecified
            let ep = Endpoint::bind("0.0.0.0:0").await.unwrap();
            let addr = ep.local_addr().unwrap();
            assert_eq!(addr.ip(), ip);
            assert_ne!(addr.port(), 0);

            // unspecified v6
            let ep = Endpoint::bind(":::0").await.unwrap();
            let addr = ep.local_addr().unwrap();
            assert_eq!(addr.ip(), ip);
            assert_ne!(addr.port(), 0);

            // localhost
            let ep = Endpoint::bind("127.0.0.1:0").await.unwrap();
            let addr = ep.local_addr().unwrap();
            assert_eq!(addr.ip().to_string(), "127.0.0.1");
            assert_ne!(addr.port(), 0);

            // localhost v6
            let ep = Endpoint::bind("::1:0").await.unwrap();
            let addr = ep.local_addr().unwrap();
            assert_eq!(addr.ip().to_string(), "::1");
            assert_ne!(addr.port(), 0);

            // wrong IP
            let err = Endpoint::bind("10.0.0.2:0").await.err().unwrap();
            assert_eq!(err.kind(), std::io::ErrorKind::AddrNotAvailable);

            // drop and reuse port
            let _ = Endpoint::bind("10.0.0.1:100").await.unwrap();
            let _ = Endpoint::bind("10.0.0.1:100").await.unwrap();
        });
        runtime.block_on(f).unwrap();
    }

    #[test]
    #[ignore]
    fn localhost() {
        let runtime = Runtime::new();
        let ip1 = "10.0.0.1".parse::<IpAddr>().unwrap();
        let ip2 = "10.0.0.2".parse::<IpAddr>().unwrap();
        let node1 = runtime.create_node().ip(ip1).build();
        let node2 = runtime.create_node().ip(ip2).build();
        let barrier = Arc::new(Barrier::new(2));

        let barrier_ = barrier.clone();
        let f1 = node1.spawn(async move {
            let ep1 = Endpoint::bind("127.0.0.1:1").await.unwrap();
            let ep2 = Endpoint::bind("10.0.0.1:2").await.unwrap();
            barrier_.wait().await;

            // FIXME: ep1 should not receive messages from other node
            timeout(Duration::from_secs(1), ep1.recv_from(1, &mut []))
                .await
                .err()
                .expect("localhost endpoint should not receive from other nodes");
            // ep2 should receive
            ep2.recv_from(1, &mut []).await.unwrap();
        });
        let f2 = node2.spawn(async move {
            let ep = Endpoint::bind("127.0.0.1:1").await.unwrap();
            barrier.wait().await;

            ep.send_to("10.0.0.1:1", 1, &[1]).await.unwrap();
            ep.send_to("10.0.0.1:2", 1, &[1]).await.unwrap();
        });
        runtime.block_on(f1).unwrap();
        runtime.block_on(f2).unwrap();
    }

    #[test]
    fn connect_send_recv() {
        let runtime = Runtime::new();
        let addr1 = "10.0.0.1:1".parse::<SocketAddr>().unwrap();
        let addr2 = "10.0.0.2:1".parse::<SocketAddr>().unwrap();
        let node1 = runtime.create_node().ip(addr1.ip()).build();
        let node2 = runtime.create_node().ip(addr2.ip()).build();
        let barrier = Arc::new(Barrier::new(2));

        let barrier_ = barrier.clone();
        node1.spawn(async move {
            let ep = Endpoint::bind(addr1).await.unwrap();
            assert_eq!(ep.local_addr().unwrap(), addr1);
            barrier_.wait().await;

            let mut buf = vec![0; 0x10];
            let (len, from) = ep.recv_from(1, &mut buf).await.unwrap();
            assert_eq!(&buf[..len], b"ping");

            ep.send_to(from, 1, b"pong").await.unwrap();
        });

        let f = node2.spawn(async move {
            barrier.wait().await;
            let ep = Endpoint::connect(addr1).await.unwrap();
            assert_eq!(ep.peer_addr().unwrap(), addr1);

            ep.send(1, b"ping").await.unwrap();

            let mut buf = vec![0; 0x10];
            let len = ep.recv(1, &mut buf).await.unwrap();
            assert_eq!(&buf[..len], b"pong");
        });

        runtime.block_on(f).unwrap();
    }
}
