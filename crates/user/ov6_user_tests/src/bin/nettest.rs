#![feature(allocator_api)]
#![cfg_attr(not(test), no_std)]

use core::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

use dataview::{DataView, Pod};
use ov6_user_lib::{
    env, eprint, eprintln,
    io::{self, Read as _, Write as _},
    net::UdpSocket,
    os::ov6::syscall,
    process::{ProcessBuilder, Stdio},
    thread,
};
use ov6_user_tests::{
    OrExit as _, exit_err,
    test_runner::{TestEntry, TestParam},
};

static SERVER_PORT: AtomicU16 = AtomicU16::new(0);

fn main() {
    let mut args = env::args().skip_while(|s| *s != "--").skip(1);

    while let Some(arg) = args.next() {
        match arg {
            "-p" => {
                let Some(s) = args.next() else {
                    eprintln!("Missing argument for -p");
                    TestParam::usage_and_exit();
                };
                let p = s.parse().or_exit(|e| {
                    exit_err!(e, "Invalid port number: `{s}`");
                });
                SERVER_PORT.store(p, Ordering::Relaxed);
            }
            _ => TestParam::usage_and_exit(),
        }
    }

    TestParam::parse().run(TESTS);
}

const TESTS: &[TestEntry] = &[
    TestEntry {
        name: "txone",
        test: txone,
        tags: &[],
    },
    TestEntry {
        name: "rx",
        test: rx,
        tags: &[],
    },
    TestEntry {
        name: "rx2",
        test: rx2,
        tags: &[],
    },
    TestEntry {
        name: "rxburst",
        test: rxburst,
        tags: &[],
    },
    TestEntry {
        name: "tx",
        test: tx,
        tags: &[],
    },
    TestEntry {
        name: "ping0",
        test: ping0,
        tags: &[],
    },
    TestEntry {
        name: "ping1",
        test: ping1,
        tags: &[],
    },
    TestEntry {
        name: "ping2",
        test: ping2,
        tags: &[],
    },
    TestEntry {
        name: "ping3",
        test: ping3,
        tags: &[],
    },
    TestEntry {
        name: "dns",
        test: dns,
        tags: &[],
    },
];

fn server_ip() -> Ipv4Addr {
    Ipv4Addr::new(10, 0, 2, 2)
}

fn server_port() -> u16 {
    let port = SERVER_PORT.load(Ordering::Relaxed);
    if port != 0 {
        return port;
    }

    option_env!("SERVER_PORT")
        .unwrap_or("0")
        .parse::<u16>()
        .unwrap()
}

fn server_addr() -> SocketAddrV4 {
    SocketAddrV4::new(server_ip(), server_port())
}

fn txone() {
    let dst = server_addr();
    let sent = syscall::send(2003, dst, b"txone").unwrap();
    assert_eq!(sent, 5);
}

fn rx() {
    let sock = UdpSocket::bind(2000).unwrap();

    let mut last_seq = None;

    for _ in 0..4 {
        let mut ibuf = [0; 128];
        let (cc, src) = sock.recv_from(&mut ibuf).unwrap();
        assert_eq!(*src.ip(), server_ip());
        let ibuf = str::from_utf8(&ibuf[..cc]).unwrap();
        let seq: usize = ibuf.strip_prefix("packet ").unwrap().parse().unwrap();
        assert!(last_seq.is_none() || last_seq.unwrap() + 1 == seq);
        last_seq = Some(seq);
        eprint!(".");
    }
}

fn rx2() {
    let sock0 = UdpSocket::bind(2000).unwrap();
    let sock1 = UdpSocket::bind(2001).unwrap();

    for _ in 0..3 {
        let mut ibuf = [0; 128];
        let (cc, src) = sock0.recv_from(&mut ibuf).unwrap();
        assert_eq!(*src.ip(), server_ip());
        let ibuf = str::from_utf8(&ibuf[..cc]).unwrap();
        assert!(ibuf.starts_with("one "));
        eprint!(".");
    }

    for _ in 0..3 {
        let mut ibuf = [0; 128];
        let (cc, src) = sock1.recv_from(&mut ibuf).unwrap();
        assert_eq!(*src.ip(), server_ip());
        let ibuf = str::from_utf8(&ibuf[..cc]).unwrap();
        assert!(ibuf.starts_with("two "));
        eprint!(".");
    }

    for _ in 0..3 {
        let mut ibuf = [0; 128];
        let (cc, src) = sock0.recv_from(&mut ibuf).unwrap();
        assert_eq!(*src.ip(), server_ip());
        let ibuf = str::from_utf8(&ibuf[..cc]).unwrap();
        assert!(ibuf.starts_with("one "));
        eprint!(".");
    }
}

fn rxburst() {
    rx();
}

fn tx() {
    let dst = server_addr();
    for i in 0..5 {
        let buf = [b't', b' ', b'0' + i];
        let sent = syscall::send(2000, dst, &buf).unwrap();
        assert_eq!(sent, 3);
        eprint!(".");
        thread::sleep(Duration::from_millis(100));
    }
}

fn ping0() {
    let sock = UdpSocket::bind(2004).unwrap();

    let dst = server_addr();
    let buf = b"ping0";
    let sent = sock.send_to(buf, dst).unwrap();
    assert_eq!(sent, buf.len());

    let mut ibuf = [0; 128];
    let (cc, src) = sock.recv_from(&mut ibuf).unwrap();
    assert_eq!(src, dst);
    let ibuf = str::from_utf8(&ibuf[..cc]).unwrap();
    assert_eq!(ibuf, "ping0");
}

fn ping1() {
    let sock = UdpSocket::bind(2005).unwrap();

    for i in 0..20 {
        let dst = server_addr();
        let buf = [b'p', b' ', b'0' + i];
        let sent = sock.send_to(&buf, dst).unwrap();
        assert_eq!(sent, buf.len());

        let mut ibuf = [0; 128];
        let (cc, src) = sock.recv_from(&mut ibuf).unwrap();
        assert_eq!(src, dst);
        assert_eq!(&ibuf[..cc], buf);
    }
}

fn ping2() {
    let sock1 = UdpSocket::bind(2006).unwrap();
    let sock2 = UdpSocket::bind(2007).unwrap();

    let dst = server_addr();

    for i in 0..5 {
        for (ch, sock) in &[(b'a', &sock1), (b'A', &sock2)] {
            let buf = [b'p', b' ', ch + i, b'!'];
            let sent = sock.send_to(&buf, dst).unwrap();
            assert_eq!(sent, buf.len());
        }
    }

    for (ch, sock) in &[(b'a', &sock1), (b'A', &sock2)] {
        for i in 0..5 {
            let mut ibuf = [0; 128];
            let (cc, src) = sock.recv_from(&mut ibuf).unwrap();
            assert_eq!(src, dst);
            assert_eq!(&ibuf[..cc], &[b'p', b' ', *ch + i, b'!']);
        }
    }
}

fn ping3() {
    let sock1 = UdpSocket::bind(2008).unwrap();
    let sock2 = UdpSocket::bind(2009).unwrap();

    let dst = server_addr();
    let buf = [b'p', b' ', b'A', b'!'];
    let sent = sock2.send_to(&buf, dst).unwrap();
    assert_eq!(sent, buf.len());

    thread::sleep(Duration::from_millis(100));

    for i in 0..=255 {
        let buf = [b'p', b' ', b'a'.wrapping_add(i), b'!'];
        let sent = sock1.send_to(&buf, dst).unwrap();
        assert_eq!(sent, buf.len());
        if i.is_multiple_of(2) {
            let sent = sock1.send_to(&buf, dst).unwrap();
            assert_eq!(sent, buf.len());
        } else {
            let sent = syscall::send(2010, dst, &buf).unwrap();
            assert_eq!(sent, buf.len());
        }
    }

    let buf = [b'p', b' ', b'B', b'!'];
    let sent = sock2.send_to(&buf, dst).unwrap();
    assert_eq!(sent, buf.len());

    for i in 0..2 {
        let mut ibuf = [0; 128];
        let (cc, src) = sock2.recv_from(&mut ibuf).unwrap();
        assert_eq!(src, dst);
        assert_eq!(&ibuf[..cc], &[b'p', b' ', b'A' + i, b'!']);
    }

    let mut child = ProcessBuilder::new()
        .stdout(Stdio::Pipe)
        .spawn_fn(|| {
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            stdout.write_all(b":").unwrap();
            stdout.flush().unwrap();
            loop {
                let mut ibuf = [0; 128];
                let (_cc, src) = sock1.recv_from(&mut ibuf).unwrap();
                assert_eq!(src, dst);
                stdout.write_all(b".").unwrap();
                stdout.flush().unwrap();
            }
        })
        .unwrap();

    thread::sleep(Duration::from_millis(500));

    let mut stdout = child.stdout.take().unwrap();
    let mut nbuf = [0; 512];
    let mut n = stdout.read(&mut nbuf).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();

    n -= 1;
    assert!(n <= 16, "ping3: too many packets received, n={n}");
}

fn dns() {
    const N: usize = 1000;
    let mut obuf = [0; N];
    let mut ibuf = [0; N];

    let dns_server = SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53);
    let req = dns_req(&mut obuf);

    let sock = UdpSocket::bind(10000).unwrap();
    let sent = sock.send_to(req, dns_server).unwrap();
    assert_eq!(sent, req.len());

    let (cc, src) = sock.recv_from(&mut ibuf).unwrap();
    assert_eq!(src, dns_server);
    let rep = &ibuf[..cc];
    dns_rep(rep);
}

fn dns_req(obuf: &mut [u8]) -> &mut [u8] {
    let mut len = 0;

    let (hdr, body) = obuf.split_at_mut(size_of::<Dns>());
    len += hdr.len();
    let hdr = DataView::from_mut(hdr).get_mut::<Dns>(0);
    hdr.set_id(6828);
    hdr.set_rd(true);
    hdr.set_qdcount(1);

    let qname = b"pdos.csail.mit.edu.";
    let qname_len = encode_qname(body, qname);
    len += qname_len;
    let body = &mut body[qname_len..];
    let body = DataView::from_mut(body).get_mut::<DnsQuestion>(0);
    len += size_of::<DnsQuestion>();
    body.set_qtype(1); // A
    body.set_qclass(1); // IN

    &mut obuf[..len]
}

fn dns_rep(rep: &[u8]) {
    let (hdr, mut body) = rep.split_at(size_of::<Dns>());
    let hdr = DataView::from(hdr).get::<Dns>(0);
    assert!(hdr.qr());
    assert_eq!(hdr.id(), 6828);
    assert_eq!(hdr.rcode(), 0);

    for _ in 0..hdr.qdcount() {
        let mut qname = [0; 128];
        let (consumed, _qname) = decode_qname(body, &mut qname);
        body = consumed;
        body = &body[size_of::<DnsQuestion>()..];
    }

    for _ in 0..hdr.ancount() {
        let mut qname = [0; 128];
        let qname = if body[0] > 63 {
            // compression?
            let off = usize::from(body[1]);
            let (_, qname) = decode_qname(&rep[off..], &mut qname);
            body = &body[2..];
            qname
        } else {
            let (consumed, qname) = decode_qname(body, &mut qname);
            body = consumed;
            qname
        };

        let (data, consumed) = body.split_at(size_of::<DnsData>());
        let data = DataView::from(data).get::<DnsData>(0);
        body = consumed;

        if data.ty() == ARECORD && data.len() == 4 {
            let ip = Ipv4Addr::new(body[0], body[1], body[2], body[3]);
            eprintln!(
                "DNS A-record for {qname} is {ip}",
                qname = str::from_utf8(qname).unwrap(),
            );
            assert_eq!(ip, Ipv4Addr::new(128, 52, 129, 126));
        }
        body = &body[usize::from(data.len())..];
    }
}

fn encode_qname(buf: &mut [u8], mut qname: &[u8]) -> usize {
    let mut len = 0;
    while !qname.is_empty() {
        let segment_len = qname.iter().take_while(|&&c| c != b'.').count();
        if segment_len == 0 {
            break;
        }
        buf[len] = u8::try_from(segment_len).unwrap();
        len += 1;
        buf[len..][..segment_len].copy_from_slice(&qname[..segment_len]);
        len += segment_len;
        qname = &qname[segment_len..]; // skip segment
        if qname.is_empty() {
            break;
        }
        qname = &qname[1..]; // skip dot if
    }
    buf[len] = 0; // Null terminator
    len + 1
}

fn decode_qname<'b, 'q>(buf: &'b [u8], qname: &'q mut [u8]) -> (&'b [u8], &'q mut [u8]) {
    let mut consumed_len = 0;
    let mut qname_len = 0;
    while buf[consumed_len] != 0 {
        if qname_len > 0 {
            qname[qname_len] = b'.';
            qname_len += 1;
        }
        let segment_len = usize::from(buf[consumed_len]);
        consumed_len += 1;
        qname[qname_len..][..segment_len].copy_from_slice(&buf[consumed_len..][..segment_len]);
        qname_len += segment_len;
        consumed_len += segment_len;
    }
    consumed_len += 1; // Null terminator
    (&buf[consumed_len..], &mut qname[..qname_len])
}

#[repr(C)]
#[derive(Debug, Pod)]
struct Dns {
    /// Request ID
    id: [u8; 2],
    /// Flags
    ///
    /// - [ 0.. 1] QR:       Query/Response
    /// - [ 1.. 5] Opcode:   Query type
    /// - [ 5.. 6] AA:       Authoritative Answer
    /// - [ 6.. 7] TC:       Truncated
    /// - [ 7.. 8] RD:       Recursion Desired
    /// - [ 8.. 9] RA:       Recursion Available
    /// - [ 9..10] Z:        Reserved for future use
    /// - [10..16] RCODE:    Response code
    flags: [u8; 2],
    /// Number of question entries
    qdcount: [u8; 2],
    /// Number of resource records in answer section
    ancount: [u8; 2],
    /// Number of name server resource records in authority section
    nscount: [u8; 2],
    /// Number of resource records in additional section
    arcount: [u8; 2],
}

#[expect(dead_code)]
impl Dns {
    fn id(&self) -> u16 {
        u16::from_be_bytes(self.id)
    }

    fn set_id(&mut self, id: u16) {
        self.id = id.to_be_bytes();
    }

    fn qr(&self) -> bool {
        let bits = self.flags[0];
        let mask = 0b1000_0000;
        (bits & mask) != 0
    }

    fn set_qr(&mut self, qr: bool) {
        let bits = &mut self.flags[0];
        let mask = 0b1000_0000;
        if qr {
            *bits |= mask;
        } else {
            *bits &= !mask;
        }
    }

    fn opcode(&self) -> u8 {
        let bits = self.flags[0];
        let mask = 0b0111_1000;
        (bits & mask) >> 3
    }

    fn set_opcode(&mut self, opcode: u8) {
        let bits = &mut self.flags[0];
        assert!(opcode < 16);
        let mask = 0b0111_1000;
        *bits &= !mask;
        *bits |= opcode << 3;
    }

    fn aa(&self) -> bool {
        let bits = self.flags[0];
        let mask = 0b0000_0100;
        (bits & mask) != 0
    }

    fn set_aa(&mut self, aa: bool) {
        let bits = &mut self.flags[0];
        let mask = 0b0000_0100;
        if aa {
            *bits |= mask;
        } else {
            *bits &= !mask;
        }
    }

    fn rd(&self) -> bool {
        let bits = self.flags[0];
        let mask = 0b0000_0001;
        (bits & mask) != 0
    }

    fn set_rd(&mut self, rd: bool) {
        let bits = &mut self.flags[0];
        let mask = 0b0000_0001;
        if rd {
            *bits |= mask;
        } else {
            *bits &= !mask;
        }
    }

    fn ra(&self) -> bool {
        let bits = self.flags[1];
        let mask = 0b1000_0000;
        (bits & mask) != 0
    }

    fn set_ra(&mut self, ra: bool) {
        let bits = &mut self.flags[1];
        let mask = 0b1000_0000;
        if ra {
            *bits |= mask;
        } else {
            *bits &= !mask;
        }
    }

    fn rcode(&self) -> u8 {
        let bits = self.flags[1];
        let mask = 0b0000_1111;
        bits & mask
    }

    fn set_rcode(&mut self, rcode: u8) {
        let bits = &mut self.flags[1];
        assert!(rcode < 16);
        let mask = 0b0000_1111;
        *bits &= !mask;
        *bits |= rcode;
    }

    fn qdcount(&self) -> u16 {
        u16::from_be_bytes(self.qdcount)
    }

    fn set_qdcount(&mut self, qdcount: u16) {
        self.qdcount = qdcount.to_be_bytes();
    }

    fn ancount(&self) -> u16 {
        u16::from_be_bytes(self.ancount)
    }

    fn set_ancount(&mut self, ancount: u16) {
        self.ancount = ancount.to_be_bytes();
    }
}

#[repr(C)]
#[derive(Debug, Pod)]
struct DnsQuestion {
    qtype: [u8; 2],
    qclass: [u8; 2],
}

impl DnsQuestion {
    fn set_qtype(&mut self, qtype: u16) {
        self.qtype = qtype.to_be_bytes();
    }

    fn set_qclass(&mut self, qclass: u16) {
        self.qclass = qclass.to_be_bytes();
    }
}

#[repr(C)]
#[derive(Debug, Pod)]
struct DnsData {
    ty: [u8; 2],
    class: [u8; 2],
    ttl: [u8; 4],
    len: [u8; 2],
}

impl DnsData {
    fn ty(&self) -> u16 {
        u16::from_be_bytes(self.ty)
    }

    fn len(&self) -> u16 {
        u16::from_be_bytes(self.len)
    }
}

const ARECORD: u16 = 1;
