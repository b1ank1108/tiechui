use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBiTCCAS+gAwIBAgIUBDwBzNjrl7Y9XSWtm7HwM6AoUkUwCgYIKoZIzj0EAwIw\n\
GjEYMBYGA1UEAwwPdGllY2h1aS10ZXN0LWNhMB4XDTI2MTExMzAzMTY1NFoXDTM2\n\
MTExMDAzMTY1NFowGjEYMBYGA1UEAwwPdGllY2h1aS10ZXN0LWNhMFkwEwYHKoZI\n\
zj0CAQYIKoZIzj0DAQcDQgAEPy7Wj3KSWZ4vTLtUs+Pnu5GXY9bpEZeZEEV8LZ54\n\
McnNzeA6w5iNNxmUTXyYM8GoJuz+MNPcWDRhhHQqIgJaZ6NTMFEwHQYDVR0OBBYE\n\
FJWXEH1vyX3Nl6RrUM+rO1K4dTBWMB8GA1UdIwQYMBaAFJWXEH1vyX3Nl6RrUM+r\n\
O1K4dTBWMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIgDlvfQPnC\n\
l5q45BA5LdLN/5mGPd1SIAX5cirDxtJZ+Y0CIQDHb3fpCGRXXUxoaB33gPyVU5hr\n\
JmWK2Fp3hddwjKlRaQ==\n\
-----END CERTIFICATE-----\n";

pub struct Socks5TestProxy {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    clients: Arc<Mutex<Vec<TcpStream>>>,
    accept_thread: Option<JoinHandle<()>>,
}

impl Socks5TestProxy {
    pub fn start(expected_auth: Option<(&str, &str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind socks5 test listener");
        let addr = listener.local_addr().expect("read socks5 listener addr");
        let auth = expected_auth.map(|(u, p)| (u.as_bytes().to_vec(), p.as_bytes().to_vec()));
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let clients = Arc::new(Mutex::new(Vec::new()));
        let hits_bg = Arc::clone(&hits);
        let stop_bg = Arc::clone(&stop);
        let clients_bg = Arc::clone(&clients);
        let accept_thread = Some(thread::spawn(move || {
            accept_loop(listener, auth, hits_bg, stop_bg, clients_bg);
        }));
        Self {
            addr,
            hits,
            stop,
            clients,
            accept_thread,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn shutdown(&mut self) {
        let Some(handle) = self.accept_thread.take() else {
            return;
        };
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        let _ = handle.join();
        if let Ok(mut guard) = self.clients.lock() {
            for stream in guard.drain(..) {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
}

impl Drop for Socks5TestProxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: TcpListener,
    auth: Option<(Vec<u8>, Vec<u8>)>,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    clients: Arc<Mutex<Vec<TcpStream>>>,
) {
    while let Ok((client, _)) = listener.accept() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if let (Ok(clone), Ok(mut guard)) = (client.try_clone(), clients.lock()) {
            guard.push(clone);
        }
        let auth = auth.clone();
        let hits = Arc::clone(&hits);
        let clients = Arc::clone(&clients);
        thread::spawn(move || handle_client(client, auth, hits, clients));
    }
}

fn handle_client(
    mut client: TcpStream,
    auth: Option<(Vec<u8>, Vec<u8>)>,
    hits: Arc<AtomicUsize>,
    clients: Arc<Mutex<Vec<TcpStream>>>,
) {
    let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = client.set_write_timeout(Some(Duration::from_secs(5)));
    if !negotiate_auth(&mut client, auth.as_ref()) {
        return;
    }
    let Ok(target) = read_connect_target(&mut client) else {
        return;
    };
    let Ok(upstream) = TcpStream::connect(&target) else {
        let _ = client.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
        return;
    };
    if let (Ok(clone), Ok(mut guard)) = (upstream.try_clone(), clients.lock()) {
        guard.push(clone);
    }
    hits.fetch_add(1, Ordering::SeqCst);
    if client
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .is_err()
    {
        return;
    }
    let _ = client.set_read_timeout(None);
    relay_bidirectional(client, upstream);
}

fn negotiate_auth(client: &mut TcpStream, auth: Option<&(Vec<u8>, Vec<u8>)>) -> bool {
    let mut ver = [0u8; 1];
    if client.read_exact(&mut ver).is_err() || ver[0] != 0x05 {
        return false;
    }
    let Ok(methods) = read_len_prefixed(client) else {
        return false;
    };
    match auth {
        Some((user, pass)) => authenticate_rfc1929(client, &methods, user, pass),
        None => select_no_auth(client, &methods),
    }
}

fn select_no_auth(client: &mut TcpStream, methods: &[u8]) -> bool {
    if !methods.contains(&0x00) {
        let _ = client.write_all(&[0x05, 0xff]);
        return false;
    }
    client.write_all(&[0x05, 0x00]).is_ok()
}

fn authenticate_rfc1929(
    client: &mut TcpStream,
    methods: &[u8],
    expected_user: &[u8],
    expected_pass: &[u8],
) -> bool {
    if !methods.contains(&0x02) {
        let _ = client.write_all(&[0x05, 0xff]);
        return false;
    }
    if client.write_all(&[0x05, 0x02]).is_err() {
        return false;
    }
    let Ok((user, pass)) = read_rfc1929_credentials(client) else {
        return false;
    };
    if user != expected_user || pass != expected_pass {
        let _ = client.write_all(&[0x01, 0x01]);
        return false;
    }
    client.write_all(&[0x01, 0x00]).is_ok()
}

fn read_rfc1929_credentials(client: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut sub_ver = [0u8; 1];
    client.read_exact(&mut sub_ver)?;
    if sub_ver[0] != 0x01 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad sub-version",
        ));
    }
    Ok((read_len_prefixed(client)?, read_len_prefixed(client)?))
}

fn read_connect_target(client: &mut TcpStream) -> io::Result<String> {
    let mut req = [0u8; 4];
    client.read_exact(&mut req)?;
    if req[0] != 0x05 || req[1] != 0x01 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad command"));
    }
    let host = read_target_host(client, req[3])?;
    let mut port = [0u8; 2];
    client.read_exact(&mut port)?;
    Ok(format!("{host}:{}", u16::from_be_bytes(port)))
}

fn read_target_host(client: &mut TcpStream, atyp: u8) -> io::Result<String> {
    match atyp {
        0x01 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip)?;
            Ok(Ipv4Addr::from(ip).to_string())
        }
        0x03 => Ok(String::from_utf8_lossy(&read_len_prefixed(client)?).into_owned()),
        0x04 => {
            let mut ip = [0u8; 16];
            client.read_exact(&mut ip)?;
            Ok(format!("[{}]", Ipv6Addr::from(ip)))
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "bad atyp")),
    }
}

fn read_len_prefixed(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 1];
    stream.read_exact(&mut len)?;
    let mut buf = vec![0u8; len[0] as usize];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn relay_bidirectional(mut client: TcpStream, mut upstream: TcpStream) {
    let Ok(mut client_reader) = client.try_clone() else {
        return;
    };
    let Ok(mut upstream_writer) = upstream.try_clone() else {
        return;
    };
    let forward = thread::spawn(move || {
        let _ = io::copy(&mut client_reader, &mut upstream_writer);
        let _ = upstream_writer.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = forward.join();
}
