extern crate event;
extern crate netutils;
extern crate rand;
extern crate syscall;

use rand::{Rng, OsRng};
use std::collections::{BTreeMap, VecDeque};
use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read, Write};
use std::{mem, process, slice, str};
use std::ops::{Deref, DerefMut};
use std::os::unix::io::FromRawFd;
use std::rc::Rc;

use event::EventQueue;
use netutils::{n16, n32, Ipv4, Ipv4Addr, Ipv4Header, Checksum};
use netutils::tcp::{Tcp, TcpHeader, TCP_FIN, TCP_SYN, TCP_RST, TCP_PSH, TCP_ACK};
use syscall::data::{Packet, TimeSpec};
use syscall::error::{Error, Result, EACCES, EADDRINUSE, EBADF, EIO, EINVAL, EISCONN, EMSGSIZE, ENOTCONN, ETIMEDOUT, EWOULDBLOCK};
use syscall::flag::{CLOCK_MONOTONIC, EVENT_READ, F_GETFL, F_SETFL, O_ACCMODE, O_CREAT, O_RDWR, O_NONBLOCK};
use syscall::scheme::SchemeMut;

fn add_time(a: &TimeSpec, b: &TimeSpec) -> TimeSpec {
    let mut secs = a.tv_sec + b.tv_sec;

    let mut nsecs = a.tv_nsec + b.tv_nsec;
    while nsecs >= 1000000000 {
        nsecs -= 1000000000;
        secs += 1;
    }

    TimeSpec {
        tv_sec: secs,
        tv_nsec: nsecs
    }
}

fn parse_socket(socket: &str) -> (Ipv4Addr, u16) {
    let mut socket_parts = socket.split(":");
    let host = Ipv4Addr::from_str(socket_parts.next().unwrap_or(""));
    let port = socket_parts.next().unwrap_or("").parse::<u16>().unwrap_or(0);
    (host, port)
}

#[derive(Debug)]
struct EmptyHandle {
    privileged: bool,
    flags: usize
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum State {
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    Closed
}

#[derive(Debug)]
struct TcpHandle {
    local: (Ipv4Addr, u16),
    remote: (Ipv4Addr, u16),
    flags: usize,
    events: usize,
    read_timeout: Option<TimeSpec>,
    write_timeout: Option<TimeSpec>,
    ttl: u8,
    state: State,
    seq: u32,
    ack: u32,
    data: VecDeque<(Ipv4, Tcp)>,
    todo_dup: VecDeque<Packet>,
    todo_read: VecDeque<(Option<TimeSpec>, Packet)>,
    todo_write: VecDeque<(Option<TimeSpec>, Packet)>,
}

impl TcpHandle {
    fn is_connected(&self) -> bool {
        self.remote.0 != Ipv4Addr::NULL && self.remote.1 != 0
    }

    fn read_closed(&self) -> bool {
        self.state == State::CloseWait || self.state == State::LastAck || self.state == State::TimeWait || self.state == State::Closed
    }

    fn matches(&self, ip: &Ipv4, tcp: &Tcp) -> bool {
        // Local address not set or IP dst matches or is broadcast
        (self.local.0 == Ipv4Addr::NULL || ip.header.dst == self.local.0 || ip.header.dst == Ipv4Addr::BROADCAST)
        // Local port matches UDP dst
        && tcp.header.dst.get() == self.local.1
        // Remote address not set or is broadcast, or IP src matches
        && (self.remote.0 == Ipv4Addr::NULL || self.remote.0 == Ipv4Addr::BROADCAST || ip.header.src == self.remote.0)
        // Remote port not set or UDP src matches
        && (self.remote.1 == 0 || tcp.header.src.get() == self.remote.1)
    }

    fn create_tcp(&self, flags: u16, data: Vec<u8>) -> Tcp {
        Tcp {
            header: TcpHeader {
                src: n16::new(self.local.1),
                dst: n16::new(self.remote.1),
                sequence: n32::new(self.seq),
                ack_num: n32::new(self.ack),
                flags: n16::new(((mem::size_of::<TcpHeader>() << 10) & 0xF000) as u16 | (flags & 0xFFF)),
                window_size: n16::new(8192),
                checksum: Checksum { data: 0 },
                urgent_pointer: n16::new(0),
            },
            options: Vec::new(),
            data: data
        }
    }

    fn create_ip(&self, id: u16, data: Vec<u8>) -> Ipv4 {
        Ipv4 {
            header: Ipv4Header {
                ver_hlen: 0x45,
                services: 0,
                len: n16::new((data.len() + mem::size_of::<Ipv4Header>()) as u16),
                id: n16::new(id),
                flags_fragment: n16::new(0),
                ttl: self.ttl,
                proto: 0x06,
                checksum: Checksum { data: 0 },
                src: self.local.0,
                dst: self.remote.0
            },
            options: Vec::new(),
            data: data
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum SettingKind {
    Ttl,
    ReadTimeout,
    WriteTimeout
}

#[derive(Debug)]
enum Handle {
    Empty(EmptyHandle),
    Tcp(TcpHandle),
    Setting(usize, SettingKind),
}

struct Tcpd {
    scheme_file: File,
    tcp_file: File,
    time_file: File,
    ports: BTreeMap<u16, usize>,
    next_id: usize,
    handles: BTreeMap<usize, Handle>,
    rng: OsRng,
}

impl Tcpd {
    fn new(scheme_file: File, tcp_file: File, time_file: File) -> Self {
        Tcpd {
            scheme_file: scheme_file,
            tcp_file: tcp_file,
            time_file: time_file,
            ports: BTreeMap::new(),
            next_id: 1,
            handles: BTreeMap::new(),
            rng: OsRng::new().expect("tcpd: failed to open RNG")
        }
    }

    fn scheme_event(&mut self) -> io::Result<()> {
        loop {
            let mut packet = Packet::default();
            if self.scheme_file.read(&mut packet)? == 0 {
                break;
            }

            let a = packet.a;
            self.handle(&mut packet);
            if packet.a == (-EWOULDBLOCK) as usize {
                if let Some(mut handle) = self.handles.get_mut(&packet.b) {
                    if let Handle::Tcp(ref mut handle) = *handle {
                        match a {
                            syscall::number::SYS_DUP => {
                                packet.a = a;
                                handle.todo_dup.push_back(packet);
                            },
                            syscall::number::SYS_READ => {
                                packet.a = a;

                                let timeout = match handle.read_timeout {
                                    Some(read_timeout) => {
                                        let mut time = TimeSpec::default();
                                        syscall::clock_gettime(CLOCK_MONOTONIC, &mut time).map_err(|err| io::Error::from_raw_os_error(err.errno))?;

                                        let timeout = add_time(&time, &read_timeout);
                                        self.time_file.write(&timeout)?;
                                        Some(timeout)
                                    },
                                    None => None
                                };

                                handle.todo_read.push_back((timeout, packet));
                            },
                            syscall::number::SYS_WRITE => {
                                packet.a = a;

                                let timeout = match handle.write_timeout {
                                    Some(write_timeout) => {
                                        let mut time = TimeSpec::default();
                                        syscall::clock_gettime(CLOCK_MONOTONIC, &mut time).map_err(|err| io::Error::from_raw_os_error(err.errno))?;

                                        let timeout = add_time(&time, &write_timeout);
                                        self.time_file.write(&timeout)?;
                                        Some(timeout)
                                    },
                                    None => None
                                };

                                handle.todo_write.push_back((timeout, packet));
                            },
                            _ => {
                                self.scheme_file.write(&packet)?;
                            }
                        }
                    }
                }
            } else {
                self.scheme_file.write(&packet)?;
            }
        }

        Ok(())
    }

    fn tcp_event(&mut self) -> io::Result<()> {
        loop {
            let mut bytes = [0; 65536];
            let count = self.tcp_file.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            if let Some(ip) = Ipv4::from_bytes(&bytes[.. count]) {
                if let Some(tcp) = Tcp::from_bytes(&ip.data) {
                    let mut closing = Vec::new();
                    let mut found_connection = false;
                    for (id, handle) in self.handles.iter_mut() {
                        if let Handle::Tcp(ref mut handle) = *handle {
                            if handle.state != State::Listen && handle.matches(&ip, &tcp) {
                                found_connection = true;

                                match handle.state {
                                    State::SynReceived => if tcp.header.flags.get() & (TCP_SYN | TCP_ACK) == TCP_ACK && tcp.header.ack_num.get() == handle.seq {
                                        handle.state = State::Established;
                                    },
                                    State::SynSent => if tcp.header.flags.get() & (TCP_SYN | TCP_ACK) == TCP_SYN | TCP_ACK && tcp.header.ack_num.get() == handle.seq {
                                        handle.state = State::Established;
                                        handle.ack = tcp.header.sequence.get() + 1;

                                        let tcp = handle.create_tcp(TCP_ACK, Vec::new());
                                        let ip = handle.create_ip(self.rng.gen(), tcp.to_bytes());
                                        self.tcp_file.write(&ip.to_bytes())?;
                                    },
                                    State::Established => if tcp.header.flags.get() & (TCP_SYN | TCP_ACK) == TCP_ACK && tcp.header.ack_num.get() == handle.seq {
                                        handle.ack = tcp.header.sequence.get();

                                        if ! tcp.data.is_empty() {
                                            handle.data.push_back((ip.clone(), tcp.clone()));
                                            handle.ack += tcp.data.len() as u32;

                                            let tcp = handle.create_tcp(TCP_ACK, Vec::new());
                                            let ip = handle.create_ip(self.rng.gen(), tcp.to_bytes());
                                            self.tcp_file.write(&ip.to_bytes())?;
                                        } else if tcp.header.flags.get() & TCP_FIN == TCP_FIN {
                                            handle.state = State::CloseWait;

                                            handle.ack += 1;

                                            let tcp = handle.create_tcp(TCP_ACK, Vec::new());
                                            let ip = handle.create_ip(self.rng.gen(), tcp.to_bytes());
                                            self.tcp_file.write(&ip.to_bytes())?;
                                        }
                                    },
                                    //TODO: Time wait
                                    State::FinWait1 => if tcp.header.flags.get() & (TCP_SYN | TCP_ACK) == TCP_ACK && tcp.header.ack_num.get() == handle.seq {
                                        handle.ack = tcp.header.sequence.get() + 1;

                                        if tcp.header.flags.get() & TCP_FIN == TCP_FIN {
                                            handle.state = State::TimeWait;

                                            let tcp = handle.create_tcp(TCP_ACK, Vec::new());
                                            let ip = handle.create_ip(self.rng.gen(), tcp.to_bytes());
                                            self.tcp_file.write(&ip.to_bytes())?;

                                            closing.push(*id);
                                        } else {
                                            handle.state = State::FinWait2;
                                        }
                                    },
                                    State::FinWait2 => if tcp.header.flags.get() & (TCP_SYN | TCP_ACK | TCP_FIN) == TCP_ACK | TCP_FIN && tcp.header.ack_num.get() == handle.seq {
                                        handle.ack = tcp.header.sequence.get() + 1;

                                        handle.state = State::TimeWait;

                                        let tcp = handle.create_tcp(TCP_ACK, Vec::new());
                                        let ip = handle.create_ip(self.rng.gen(), tcp.to_bytes());
                                        self.tcp_file.write(&ip.to_bytes())?;

                                        closing.push(*id);
                                    },
                                    State::LastAck => if tcp.header.flags.get() & (TCP_SYN | TCP_ACK) == TCP_ACK && tcp.header.ack_num.get() == handle.seq {
                                        handle.state = State::Closed;
                                        closing.push(*id);
                                    },
                                    _ => ()
                                }

                                while ! handle.todo_read.is_empty() && (! handle.data.is_empty() || handle.read_closed()) {
                                    let (_timeout, mut packet) = handle.todo_read.pop_front().unwrap();
                                    let buf = unsafe { slice::from_raw_parts_mut(packet.c as *mut u8, packet.d) };
                                    if let Some((ip, mut tcp)) = handle.data.pop_front() {
                                        let len = std::cmp::min(buf.len(), tcp.data.len());
                                        for (i, c) in tcp.data.drain(0..len).enumerate() {
                                            buf[i] = c;
                                        }
                                        if !tcp.data.is_empty() {
                                            handle.data.push_front((ip, tcp));
                                        }
                                        packet.a = len;
                                    } else {
                                        packet.a = 0;
                                    }

                                    self.scheme_file.write(&packet)?;
                                }

                                if ! handle.todo_write.is_empty() && handle.state == State::Established {
                                    let (_timeout, mut packet) = handle.todo_write.pop_front().unwrap();
                                    let buf = unsafe { slice::from_raw_parts(packet.c as *const u8, packet.d) };

                                    let tcp = handle.create_tcp(TCP_ACK | TCP_PSH, buf.to_vec());
                                    let ip = handle.create_ip(self.rng.gen(), tcp.to_bytes());
                                    let result = self.tcp_file.write(&ip.to_bytes()).map_err(|err| Error::new(err.raw_os_error().unwrap_or(EIO)));
                                    if result.is_ok() {
                                        handle.seq += buf.len() as u32;
                                    }
                                    packet.a = Error::mux(result.and(Ok(buf.len())));

                                    self.scheme_file.write(&packet)?;
                                }

                                if handle.events & EVENT_READ == EVENT_READ {
                                    if let Some(&(ref _ip, ref tcp)) = handle.data.get(0) {
                                        self.scheme_file.write(&Packet {
                                            id: 0,
                                            pid: 0,
                                            uid: 0,
                                            gid: 0,
                                            a: syscall::number::SYS_FEVENT,
                                            b: *id,
                                            c: EVENT_READ,
                                            d: tcp.data.len()
                                        })?;
                                    }
                                }
                            }
                        }
                    }

                    for file in closing {
                        if let Handle::Tcp(handle) = self.handles.remove(&file).unwrap() {
                            let remove = if let Some(mut port) = self.ports.get_mut(&handle.local.1) {
                                *port = *port + 1;
                                *port == 0
                            } else {
                                false
                            };

                            if remove {
                                self.ports.remove(&handle.local.1);
                            }
                        }
                    }

                    if ! found_connection && tcp.header.flags.get() & (TCP_SYN | TCP_ACK) == TCP_SYN {
                        let mut new_handles = Vec::new();

                        for (id, handle) in self.handles.iter_mut() {
                            if let Handle::Tcp(ref mut handle) = *handle {
                                if handle.state == State::Listen && handle.matches(&ip, &tcp) {
                                    handle.data.push_back((ip.clone(), tcp.clone()));

                                    while ! handle.todo_dup.is_empty() && ! handle.data.is_empty() {
                                        let mut packet = handle.todo_dup.pop_front().unwrap();
                                        let (ip, tcp) = handle.data.pop_front().unwrap();

                                        let mut new_handle = TcpHandle {
                                            local: handle.local,
                                            remote: (ip.header.src, tcp.header.src.get()),
                                            flags: handle.flags,
                                            events: 0,
                                            read_timeout: handle.read_timeout,
                                            write_timeout: handle.write_timeout,
                                            ttl: handle.ttl,
                                            state: State::SynReceived,
                                            seq: self.rng.gen(),
                                            ack: tcp.header.sequence.get() + 1,
                                            data: VecDeque::new(),
                                            todo_dup: VecDeque::new(),
                                            todo_read: VecDeque::new(),
                                            todo_write: VecDeque::new(),
                                        };

                                        let tcp = new_handle.create_tcp(TCP_SYN | TCP_ACK, Vec::new());
                                        let ip = new_handle.create_ip(self.rng.gen(), tcp.to_bytes());
                                        self.tcp_file.write(&ip.to_bytes())?;

                                        new_handle.seq += 1;

                                        handle.data.retain(|&(ref ip, ref tcp)| {
                                            if new_handle.matches(ip, tcp) {
                                                false
                                            } else {
                                                true
                                            }
                                        });

                                        if let Some(mut port) = self.ports.get_mut(&handle.local.1) {
                                            *port = *port + 1;
                                        }

                                        let id = self.next_id;
                                        self.next_id += 1;

                                        packet.a = id;

                                        new_handles.push((packet, Handle::Tcp(new_handle)));
                                    }

                                    if handle.events & EVENT_READ == EVENT_READ {
                                        if let Some(&(ref _ip, ref tcp)) = handle.data.get(0) {
                                            self.scheme_file.write(&Packet {
                                                id: 0,
                                                pid: 0,
                                                uid: 0,
                                                gid: 0,
                                                a: syscall::number::SYS_FEVENT,
                                                b: *id,
                                                c: EVENT_READ,
                                                d: tcp.data.len()
                                            })?;
                                        }
                                    }
                                }
                            }
                        }

                        for (packet, new_handle) in new_handles {
                            self.handles.insert(packet.a, new_handle);
                            self.scheme_file.write(&packet)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn time_event(&mut self) -> io::Result<()> {
        let mut time = TimeSpec::default();
        if self.time_file.read(&mut time)? < mem::size_of::<TimeSpec>() {
            return Err(io::Error::from_raw_os_error(EINVAL));
        }

        for (_id, handle) in self.handles.iter_mut() {
            if let Handle::Tcp(ref mut handle) = *handle {
                let mut i = 0;
                while i < handle.todo_read.len() {
                    if let Some(timeout) =  handle.todo_read.get(i).map(|e| e.0.clone()).unwrap_or(None) {
                        if time.tv_sec > timeout.tv_sec || (time.tv_sec == timeout.tv_sec && time.tv_nsec >= timeout.tv_nsec) {
                            let (_timeout, mut packet) = handle.todo_read.remove(i).unwrap();
                            packet.a = (-ETIMEDOUT) as usize;
                            self.scheme_file.write(&packet)?;
                        } else {
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }

                let mut i = 0;
                while i < handle.todo_write.len() {
                    if let Some(timeout) = handle.todo_write.get(i).map(|e| e.0.clone()).unwrap_or(None) {
                        if time.tv_sec > timeout.tv_sec || (time.tv_sec == timeout.tv_sec && time.tv_nsec >= timeout.tv_nsec) {
                            let (_timeout, mut packet) = handle.todo_write.remove(i).unwrap();
                            packet.a = (-ETIMEDOUT) as usize;
                            self.scheme_file.write(&packet)?;
                        } else {
                            i += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
        }

        Ok(())
    }

    fn inner_dup(&mut self, file: usize, path: &str) -> Result<Handle> {
        Ok(match *self.handles.get_mut(&file).ok_or(Error::new(EBADF))? {
            Handle::Empty(ref handle) => {
                if path.is_empty() {
                    Handle::Empty(EmptyHandle {
                        privileged: handle.privileged,
                        flags: handle.flags
                    })
                } else {
                    let mut parts = path.split("/");
                    let remote = parse_socket(parts.next().unwrap_or(""));
                    let mut local = parse_socket(parts.next().unwrap_or(""));

                    if local.1 == 0 {
                        local.1 = self.rng.gen_range(32768, 65535);
                    }

                    if local.1 <= 1024 && ! handle.privileged {
                        return Err(Error::new(EACCES));
                    }

                    if self.ports.contains_key(&local.1) {
                        return Err(Error::new(EADDRINUSE));
                    }

                    let mut new_handle = TcpHandle {
                        local: local,
                        remote: remote,
                        flags: handle.flags,
                        events: 0,
                        read_timeout: None,
                        write_timeout: None,
                        ttl: 64,
                        state: State::Listen,
                        seq: 0,
                        ack: 0,
                        data: VecDeque::new(),
                        todo_dup: VecDeque::new(),
                        todo_read: VecDeque::new(),
                        todo_write: VecDeque::new(),
                    };

                    if new_handle.is_connected() {
                        new_handle.seq = self.rng.gen();
                        new_handle.ack = 0;
                        new_handle.state = State::SynSent;

                        let tcp = new_handle.create_tcp(TCP_SYN, Vec::new());
                        let ip = new_handle.create_ip(self.rng.gen(), tcp.to_bytes());
                        self.tcp_file.write(&ip.to_bytes()).map_err(|err| Error::new(err.raw_os_error().unwrap_or(EIO)))?;

                        new_handle.seq += 1;
                    }

                    self.ports.insert(new_handle.local.1, 1);

                    Handle::Tcp(new_handle)
                }
            },
            Handle::Tcp(ref mut handle) => {
                let mut new_handle = TcpHandle {
                    local: handle.local,
                    remote: handle.remote,
                    flags: handle.flags,
                    events: 0,
                    read_timeout: handle.read_timeout,
                    write_timeout: handle.write_timeout,
                    ttl: handle.ttl,
                    state: handle.state,
                    seq: handle.seq,
                    ack: handle.ack,
                    data: VecDeque::new(),
                    todo_dup: VecDeque::new(),
                    todo_read: VecDeque::new(),
                    todo_write: VecDeque::new(),
                };

                if path == "ttl" {
                    Handle::Setting(file, SettingKind::Ttl)
                } else if path == "read_timeout" {
                    Handle::Setting(file, SettingKind::ReadTimeout)
                } else if path == "write_timeout" {
                    Handle::Setting(file, SettingKind::WriteTimeout)
                } else if path == "listen" {
                    if handle.is_connected() {
                        return Err(Error::new(EISCONN));
                    } else if let Some((ip, tcp)) = handle.data.pop_front() {
                        new_handle.remote = (ip.header.src, tcp.header.src.get());

                        new_handle.seq = self.rng.gen();
                        new_handle.ack = tcp.header.sequence.get() + 1;
                        new_handle.state = State::SynReceived;

                        let tcp = new_handle.create_tcp(TCP_SYN | TCP_ACK, Vec::new());
                        let ip = new_handle.create_ip(self.rng.gen(), tcp.to_bytes());
                        self.tcp_file.write(&ip.to_bytes()).map_err(|err| Error::new(err.raw_os_error().unwrap_or(EIO)))?;

                        new_handle.seq += 1;
                    } else {
                        return Err(Error::new(EWOULDBLOCK));
                    }

                    handle.data.retain(|&(ref ip, ref tcp)| {
                        if new_handle.matches(ip, tcp) {
                            false
                        } else {
                            true
                        }
                    });

                    Handle::Tcp(new_handle)
                } else if path.is_empty() {
                    new_handle.data = handle.data.clone();

                    Handle::Tcp(new_handle)
                } else {
                    return Err(Error::new(EINVAL));
                }
            },
            Handle::Setting(file, kind) => {
                Handle::Setting(file, kind)
            }
        })
    }
}

impl SchemeMut for Tcpd {
    fn open(&mut self, url: &[u8], flags: usize, uid: u32, _gid: u32) -> Result<usize> {
        let path = str::from_utf8(url).or(Err(Error::new(EINVAL)))?;

        let id = self.next_id;
        self.next_id += 1;

        self.handles.insert(id, Handle::Empty(EmptyHandle {
            privileged: uid == 0,
            flags: flags
        }));

        match self.inner_dup(id, path) {
            Ok(handle) => {
                self.handles.insert(id, handle);
                Ok(id)
            },
            Err(err) => {
                self.handles.remove(&id);
                Err(err)
            }
        }
    }

    fn dup(&mut self, file: usize, buf: &[u8]) -> Result<usize> {
        let path = str::from_utf8(buf).or(Err(Error::new(EINVAL)))?;

        let handle = self.inner_dup(file, path)?;

        let id = self.next_id;
        self.next_id += 1;

        self.handles.insert(id, handle);

        Ok(id)
    }

    fn read(&mut self, file: usize, buf: &mut [u8]) -> Result<usize> {
        let (file, kind) = match *self.handles.get_mut(&file).ok_or(Error::new(EBADF))? {
            Handle::Empty(ref _handle) => {
                return Err(Error::new(EBADF));
            },
            Handle::Tcp(ref mut handle) => {
                if ! handle.is_connected() {
                    return Err(Error::new(ENOTCONN));
                } else if let Some((ip, mut tcp)) = handle.data.pop_front() {
                    let len = std::cmp::min(buf.len(), tcp.data.len());
                    for (i, c) in tcp.data.drain(0..len).enumerate() {
                        buf[i] = c;
                    }
                    if !tcp.data.is_empty() {
                        handle.data.push_front((ip, tcp));
                    }

                    return Ok(len);
                } else if handle.flags & O_NONBLOCK == O_NONBLOCK || handle.read_closed() {
                    return Ok(0);
                } else {
                    return Err(Error::new(EWOULDBLOCK));
                }
            },
            Handle::Setting(file, kind) => {
                (file, kind)
            }
        };

        if let Handle::Tcp(ref mut handle) = *self.handles.get_mut(&file).ok_or(Error::new(EBADF))? {
            let get_timeout = |timeout: &Option<TimeSpec>, buf: &mut [u8]| -> Result<usize> {
                if let Some(ref timespec) = *timeout {
                    timespec.deref().read(buf).map_err(|err| Error::new(err.raw_os_error().unwrap_or(EIO)))
                } else {
                    Ok(0)
                }
            };

            match kind {
                SettingKind::Ttl => {
                    if let Some(mut ttl) = buf.get_mut(0) {
                        *ttl = handle.ttl;
                        Ok(1)
                    } else {
                        Ok(0)
                    }
                },
                SettingKind::ReadTimeout => {
                    get_timeout(&handle.read_timeout, buf)
                },
                SettingKind::WriteTimeout => {
                    get_timeout(&handle.write_timeout, buf)
                }
            }
        } else {
            Err(Error::new(EBADF))
        }
    }

    fn write(&mut self, file: usize, buf: &[u8]) -> Result<usize> {
        let (file, kind) = match *self.handles.get_mut(&file).ok_or(Error::new(EBADF))? {
            Handle::Empty(ref _handle) => {
                return Err(Error::new(EBADF));
            },
            Handle::Tcp(ref mut handle) => {
                if ! handle.is_connected() {
                    return Err(Error::new(ENOTCONN));
                } else if buf.len() >= 65507 {
                    return Err(Error::new(EMSGSIZE));
                } else {
                    match handle.state {
                        State::Established => {
                            let tcp = handle.create_tcp(TCP_ACK | TCP_PSH, buf.to_vec());
                            let ip = handle.create_ip(self.rng.gen(), tcp.to_bytes());
                            self.tcp_file.write(&ip.to_bytes()).map_err(|err| Error::new(err.raw_os_error().unwrap_or(EIO)))?;
                            handle.seq += buf.len() as u32;
                            return Ok(buf.len());
                        },
                        _ => {
                            return Err(Error::new(EWOULDBLOCK));
                        }
                    }
                }
            },
            Handle::Setting(file, kind) => {
                (file, kind)
            }
        };

        if let Handle::Tcp(ref mut handle) = *self.handles.get_mut(&file).ok_or(Error::new(EBADF))? {
            let set_timeout = |timeout: &mut Option<TimeSpec>, buf: &[u8]| -> Result<usize> {
                if buf.len() >= mem::size_of::<TimeSpec>() {
                    let mut timespec = TimeSpec::default();
                    let count = timespec.deref_mut().write(buf).map_err(|err| Error::new(err.raw_os_error().unwrap_or(EIO)))?;
                    *timeout = Some(timespec);
                    Ok(count)
                } else {
                    *timeout = None;
                    Ok(0)
                }
            };

            match kind {
                SettingKind::Ttl => {
                    if let Some(ttl) = buf.get(0) {
                        handle.ttl = *ttl;
                        Ok(1)
                    } else {
                        Ok(0)
                    }
                },
                SettingKind::ReadTimeout => {
                    set_timeout(&mut handle.read_timeout, buf)
                },
                SettingKind::WriteTimeout => {
                    set_timeout(&mut handle.write_timeout, buf)
                }
            }
        } else {
            Err(Error::new(EBADF))
        }
    }

    fn fcntl(&mut self, file: usize, cmd: usize, arg: usize) -> Result<usize> {
        match *self.handles.get_mut(&file).ok_or(Error::new(EBADF))? {
            Handle::Empty(ref mut handle) => {
                match cmd {
                    F_GETFL => Ok(handle.flags),
                    F_SETFL => {
                        handle.flags = arg & ! O_ACCMODE;
                        Ok(0)
                    },
                    _ => Err(Error::new(EINVAL))
                }
            },
            Handle::Tcp(ref mut handle) => {
                match cmd {
                    F_GETFL => Ok(handle.flags),
                    F_SETFL => {
                        handle.flags = arg & ! O_ACCMODE;
                        Ok(0)
                    },
                    _ => Err(Error::new(EINVAL))
                }
            }
            _ => Err(Error::new(EBADF))
        }
    }

    fn fevent(&mut self, file: usize, flags: usize) -> Result<usize> {
        if let Handle::Tcp(ref mut handle) = *self.handles.get_mut(&file).ok_or(Error::new(EBADF))? {
            handle.events = flags;
            Ok(file)
        } else {
            Err(Error::new(EBADF))
        }
    }

    fn fpath(&mut self, file: usize, buf: &mut [u8]) -> Result<usize> {
        if let Handle::Tcp(ref mut handle) = *self.handles.get_mut(&file).ok_or(Error::new(EBADF))? {
            let path_string = format!("tcp:{}:{}/{}:{}", handle.remote.0.to_string(), handle.remote.1, handle.local.0.to_string(), handle.local.1);
            let path = path_string.as_bytes();

            let mut i = 0;
            while i < buf.len() && i < path.len() {
                buf[i] = path[i];
                i += 1;
            }

            Ok(i)
        } else {
            Err(Error::new(EBADF))
        }
    }

    fn fsync(&mut self, file: usize) -> Result<usize> {
        let _handle = self.handles.get(&file).ok_or(Error::new(EBADF))?;

        Ok(0)
    }

    fn close(&mut self, file: usize) -> Result<usize> {
        let closed = {
            if let Handle::Tcp(ref mut handle) = *self.handles.get_mut(&file).ok_or(Error::new(EBADF))? {
                handle.data.clear();

                match handle.state {
                    State::SynReceived | State::Established => {
                        handle.state = State::FinWait1;

                        let tcp = handle.create_tcp(TCP_FIN | TCP_ACK, Vec::new());
                        let ip = handle.create_ip(self.rng.gen(), tcp.to_bytes());
                        self.tcp_file.write(&ip.to_bytes()).map_err(|err| Error::new(err.raw_os_error().unwrap_or(EIO)))?;

                        handle.seq += 1;

                        false
                    },
                    State::CloseWait => {
                        handle.state = State::LastAck;

                        let tcp = handle.create_tcp(TCP_FIN | TCP_ACK, Vec::new());
                        let ip = handle.create_ip(self.rng.gen(), tcp.to_bytes());
                        self.tcp_file.write(&ip.to_bytes()).map_err(|err| Error::new(err.raw_os_error().unwrap_or(EIO)))?;

                        handle.seq += 1;

                        false
                    },
                    _ => true
                }
            } else {
                true
            }
        };

        if closed {
            if let Handle::Tcp(handle) = self.handles.remove(&file).ok_or(Error::new(EBADF))? {
                let remove = if let Some(mut port) = self.ports.get_mut(&handle.local.1) {
                    *port = *port + 1;
                    *port == 0
                } else {
                    false
                };

                if remove {
                    self.ports.remove(&handle.local.1);
                }
            }
        }

        Ok(0)
    }
}

fn daemon(scheme_fd: usize, tcp_fd: usize, time_fd: usize) {
    let scheme_file = unsafe { File::from_raw_fd(scheme_fd) };
    let tcp_file = unsafe { File::from_raw_fd(tcp_fd) };
    let time_file = unsafe { File::from_raw_fd(time_fd) };

    let tcpd = Rc::new(RefCell::new(Tcpd::new(scheme_file, tcp_file, time_file)));

    let mut event_queue = EventQueue::<()>::new().expect("tcpd: failed to create event queue");

    let time_tcpd = tcpd.clone();
    event_queue.add(time_fd, move |_count: usize| -> io::Result<Option<()>> {
        time_tcpd.borrow_mut().time_event()?;
        Ok(None)
    }).expect("tcpd: failed to listen to events on time:");

    let tcp_tcpd = tcpd.clone();
    event_queue.add(tcp_fd, move |_count: usize| -> io::Result<Option<()>> {
        tcp_tcpd.borrow_mut().tcp_event()?;
        Ok(None)
    }).expect("tcpd: failed to listen to events on ip:6");

    event_queue.add(scheme_fd, move |_count: usize| -> io::Result<Option<()>> {
        tcpd.borrow_mut().scheme_event()?;
        Ok(None)
    }).expect("tcpd: failed to listen to events on :tcp");

    event_queue.trigger_all(0).expect("tcpd: failed to trigger event queue");

    event_queue.run().expect("tcpd: failed to run event queue");
}

fn main() {
    let time_path = format!("time:{}", CLOCK_MONOTONIC);
    match syscall::open(&time_path, O_RDWR) {
        Ok(time_fd) => {
            println!("tcpd: opening ip:6");
            match syscall::open("ip:6", O_RDWR | O_NONBLOCK) {
                Ok(tcp_fd) => {
                    // Daemonize
                    if unsafe { syscall::clone(0).unwrap() } == 0 {
                        println!("tcpd: providing tcp:");
                        match syscall::open(":tcp", O_RDWR | O_CREAT | O_NONBLOCK) {
                            Ok(scheme_fd) => {
                                daemon(scheme_fd, tcp_fd, time_fd);
                            },
                            Err(err) => {
                                println!("tcpd: failed to create tcp scheme: {}", err);
                                process::exit(1);
                            }
                        }
                    }
                },
                Err(err) => {
                    println!("tcpd: failed to open ip:6: {}", err);
                    process::exit(1);
                }
            }
        },
        Err(err) => {
            println!("tcpd: failed to open {}: {}", time_path, err);
            process::exit(1);
        }
    }
}
