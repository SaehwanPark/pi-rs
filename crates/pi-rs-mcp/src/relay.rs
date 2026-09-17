//! Per-request local proxy for aborting a blocking ureq exchange.
//!
//! ureq has no cancellation handle while waiting for response headers. The
//! one-shot relay gives the caller a local socket it can close; the worker then
//! observes EOF and can be joined promptly without sending a second JSON-RPC
//! request. A per-attempt nonce protects the localhost capability socket, and
//! configured HTTP proxy routes are chained rather than bypassed.

use std::{
  fs,
  io::{self, Read, Write},
  net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread,
  time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(25);
const MAX_REQUEST_HEADERS: usize = 2 * 1024 * 1024;
const MAX_PROXY_RESPONSE_HEADERS: usize = 64 * 1024;

#[derive(Clone, Debug)]
struct UpstreamProxy {
  host: String,
  port: u16,
  authorization: Option<String>,
}

pub(crate) struct CancellableHttpRelay {
  address: SocketAddr,
  nonce: String,
  stop: Arc<AtomicBool>,
  handle: Option<thread::JoinHandle<()>>,
}

impl CancellableHttpRelay {
  pub(crate) fn start(url: &str, connect_timeout: Duration) -> io::Result<Self> {
    let target = target_from_url(url).ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        "MCP URL has no usable authority",
      )
    })?;
    let upstream = configured_upstream_proxy(&target)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let nonce = pi_rs_core::ids::uuidv7();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_nonce = nonce.clone();
    let handle = thread::Builder::new()
      .name("mcp-http-relay".into())
      .spawn(move || {
        run(
          listener,
          target,
          upstream,
          connect_timeout,
          thread_nonce,
          thread_stop,
        )
      })?;
    Ok(Self {
      address,
      nonce,
      stop,
      handle: Some(handle),
    })
  }

  pub(crate) fn proxy_url(&self) -> String {
    format!("http://{}", self.address)
  }

  pub(crate) fn nonce(&self) -> &str {
    &self.nonce
  }

  pub(crate) fn stop(&mut self) {
    self.stop.store(true, Ordering::Release);
    let _ = TcpStream::connect(self.address);
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
  }
}

impl Drop for CancellableHttpRelay {
  fn drop(&mut self) {
    self.stop();
  }
}

fn run(
  listener: TcpListener,
  target: (String, u16),
  upstream: Option<UpstreamProxy>,
  connect_timeout: Duration,
  nonce: String,
  stop: Arc<AtomicBool>,
) {
  while !stop.load(Ordering::Acquire) {
    match listener.accept() {
      Ok((client, _)) => {
        if serve(
          client,
          &target,
          upstream.as_ref(),
          connect_timeout,
          &nonce,
          &stop,
        ) {
          return;
        }
      }
      Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
      Err(_) => return,
    }
  }
}

fn serve(
  mut client: TcpStream,
  target: &(String, u16),
  upstream: Option<&UpstreamProxy>,
  connect_timeout: Duration,
  nonce: &str,
  stop: &Arc<AtomicBool>,
) -> bool {
  let _ = client.set_read_timeout(Some(POLL));
  let _ = client.set_write_timeout(Some(POLL));
  let mut request = Vec::new();
  let header_deadline = Instant::now() + connect_timeout.max(POLL).min(Duration::from_secs(1));
  let header_end = loop {
    if stop.load(Ordering::Acquire) || Instant::now() >= header_deadline {
      return false;
    }
    let mut chunk = [0u8; 16 * 1024];
    match client.read(&mut chunk) {
      Ok(0) => return false,
      Ok(count) => {
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
          break end + 4;
        }
        if request.len() > MAX_REQUEST_HEADERS {
          return false;
        }
      }
      Err(error)
        if matches!(
          error.kind(),
          io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) =>
      {
        continue;
      }
      Err(_) => return false,
    }
  };

  // The relay is a localhost capability endpoint. Require a per-attempt
  // bearer nonce before consuming its one allowed exchange; an unrelated local
  // process can otherwise win the accept race and turn cancellation into a
  // denial-of-service against the real MCP request.
  if !has_nonce(&request[..header_end], nonce) {
    return false;
  }

  let Some(mut remote) = connect_remote(target, upstream, connect_timeout, stop) else {
    return false;
  };
  let _ = remote.set_read_timeout(Some(POLL));
  let _ = remote.set_write_timeout(Some(POLL));
  let connect_request = request
    .get(..header_end)
    .and_then(|header| header.split(|byte| *byte == b'\n').next())
    .is_some_and(|line| line.starts_with(b"CONNECT "));
  if connect_request {
    if client
      .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
      .is_err()
    {
      return false;
    }
  } else {
    let request = origin_form(&request, header_end);
    if remote.write_all(&request).is_err() {
      return false;
    }
  }
  if connect_request
    && request.len() > header_end
    && remote.write_all(&request[header_end..]).is_err()
  {
    return false;
  }
  relay_bidirectionally(client, remote, stop);
  true
}

fn has_nonce(headers: &[u8], expected: &str) -> bool {
  let user_agent = format!("pi-rs-relay/{expected}");
  headers.split(|byte| *byte == b'\n').any(|line| {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(separator) = line.iter().position(|byte| *byte == b':') else {
      return false;
    };
    let (name, value) = line.split_at(separator);
    (name.eq_ignore_ascii_case(b"x-pi-rs-relay-nonce")
      && trim_ascii(&value[1..]) == expected.as_bytes())
      || (name.eq_ignore_ascii_case(b"user-agent")
        && trim_ascii(&value[1..]) == user_agent.as_bytes())
  })
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
  let start = bytes
    .iter()
    .position(|byte| !byte.is_ascii_whitespace())
    .unwrap_or(bytes.len());
  let end = bytes
    .iter()
    .rposition(|byte| !byte.is_ascii_whitespace())
    .map_or(start, |index| index + 1);
  &bytes[start..end]
}

fn connect_remote(
  target: &(String, u16),
  upstream: Option<&UpstreamProxy>,
  connect_timeout: Duration,
  stop: &Arc<AtomicBool>,
) -> Option<TcpStream> {
  if let Some(proxy) = upstream {
    let mut stream = connect_host(&proxy.host, proxy.port, connect_timeout, stop)?;
    if establish_proxy_tunnel(&mut stream, target, proxy, stop) {
      return Some(stream);
    }
    return None;
  }
  connect_host(&target.0, target.1, connect_timeout, stop)
}

fn connect_host(
  host: &str,
  port: u16,
  connect_timeout: Duration,
  stop: &Arc<AtomicBool>,
) -> Option<TcpStream> {
  let deadline = Instant::now() + connect_timeout.max(POLL);
  let addresses = resolve_target(host, port, deadline, stop)?;
  for address in addresses {
    loop {
      if stop.load(Ordering::Acquire) {
        return None;
      }
      let remaining = deadline.saturating_duration_since(Instant::now());
      if remaining.is_zero() {
        return None;
      }
      match TcpStream::connect_timeout(&address, remaining.min(POLL)) {
        Ok(stream) => return Some(stream),
        Err(error)
          if matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
          ) =>
        {
          continue;
        }
        Err(_) => break,
      }
    }
  }
  None
}

fn establish_proxy_tunnel(
  stream: &mut TcpStream,
  target: &(String, u16),
  proxy: &UpstreamProxy,
  stop: &Arc<AtomicBool>,
) -> bool {
  let _ = stream.set_read_timeout(Some(POLL));
  let _ = stream.set_write_timeout(Some(POLL));
  let authority = format_authority(&target.0, target.1);
  let mut request = format!(
    "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n"
  );
  if let Some(authorization) = &proxy.authorization {
    request.push_str("Proxy-Authorization: ");
    request.push_str(authorization);
    request.push_str("\r\n");
  }
  request.push_str("\r\n");
  if stream.write_all(request.as_bytes()).is_err() {
    return false;
  }
  let mut response = Vec::new();
  loop {
    if stop.load(Ordering::Acquire) || response.len() > MAX_PROXY_RESPONSE_HEADERS {
      return false;
    }
    let mut chunk = [0u8; 4096];
    match stream.read(&mut chunk) {
      Ok(0) => return false,
      Ok(count) => {
        response.extend_from_slice(&chunk[..count]);
        if let Some(end) = response.windows(4).position(|window| window == b"\r\n\r\n") {
          let status = response
            .get(..end)
            .and_then(|head| head.split(|byte| *byte == b'\n').next())
            .and_then(|line| line.split(|byte| *byte == b' ').nth(1));
          return status.is_some_and(|code| code.len() == 3 && code[0] == b'2');
        }
      }
      Err(error)
        if matches!(
          error.kind(),
          io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) =>
      {
        continue;
      }
      Err(_) => return false,
    }
  }
}

fn format_authority(host: &str, port: u16) -> String {
  if host.contains(':') && !host.starts_with('[') {
    format!("[{host}]:{port}")
  } else {
    format!("{host}:{port}")
  }
}

fn resolve_target(
  host: &str,
  port: u16,
  deadline: Instant,
  stop: &Arc<AtomicBool>,
) -> Option<Vec<SocketAddr>> {
  if let Ok(address) = host.parse::<IpAddr>() {
    return Some(vec![SocketAddr::new(address, port)]);
  }
  if let Some(addresses) = hosts_file_addresses(host, port) {
    return Some(addresses);
  }
  let nameservers = resolver_addresses();
  let mut addresses = Vec::new();
  for query_type in [1u16, 28u16] {
    for nameserver in &nameservers {
      if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
        return None;
      }
      addresses.extend(query_dns(host, *nameserver, query_type, deadline, stop));
    }
  }
  addresses.sort_unstable();
  addresses.dedup();
  (!addresses.is_empty()).then_some(
    addresses
      .into_iter()
      .map(|address| SocketAddr::new(address, port))
      .collect(),
  )
}

fn hosts_file_addresses(host: &str, port: u16) -> Option<Vec<SocketAddr>> {
  let path = if cfg!(windows) {
    std::env::var_os("SystemRoot")
      .map(std::path::PathBuf::from)
      .map(|root| root.join("System32\\drivers\\etc\\hosts"))?
  } else {
    std::path::PathBuf::from("/etc/hosts")
  };
  let text = fs::read_to_string(path).ok()?;
  let mut addresses = Vec::new();
  for line in text.lines() {
    let fields: Vec<&str> = line.split('#').next()?.split_whitespace().collect();
    let Some((address, names)) = fields.split_first() else {
      continue;
    };
    if names.iter().any(|name| name.eq_ignore_ascii_case(host))
      && let Ok(address) = address.parse::<IpAddr>()
    {
      addresses.push(SocketAddr::new(address, port));
    }
  }
  (!addresses.is_empty()).then_some(addresses)
}

fn resolver_addresses() -> Vec<SocketAddr> {
  let mut addresses = Vec::new();
  #[cfg(not(windows))]
  if let Ok(text) = fs::read_to_string("/etc/resolv.conf") {
    for line in text.lines() {
      let Some(value) = line.split('#').next().and_then(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some("nameserver"))
          .then(|| fields.next())
          .flatten()
      }) else {
        continue;
      };
      if let Ok(address) = value.parse::<IpAddr>() {
        addresses.push(SocketAddr::new(address, 53));
      }
    }
  }
  if addresses.is_empty() {
    addresses.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53));
  }
  addresses
}

fn query_dns(
  host: &str,
  nameserver: SocketAddr,
  query_type: u16,
  deadline: Instant,
  stop: &Arc<AtomicBool>,
) -> Vec<IpAddr> {
  let Some(query) = dns_query(host, query_type) else {
    return Vec::new();
  };
  let bind = match nameserver {
    SocketAddr::V4(_) => "0.0.0.0:0",
    SocketAddr::V6(_) => "[::]:0",
  };
  let Ok(socket) = UdpSocket::bind(bind) else {
    return Vec::new();
  };
  if socket.set_nonblocking(true).is_err() || socket.send_to(&query, nameserver).is_err() {
    return Vec::new();
  }
  let query_id = u16::from_be_bytes([query[0], query[1]]);
  let mut response = [0u8; 4096];
  while !stop.load(Ordering::Acquire) {
    if Instant::now() >= deadline {
      break;
    }
    match socket.recv_from(&mut response) {
      Ok((length, source)) if source.ip() == nameserver.ip() => {
        return dns_answers(&response[..length], query_id, query_type);
      }
      Ok(_) => {}
      Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
      Err(_) => break,
    }
  }
  Vec::new()
}

fn dns_query(host: &str, query_type: u16) -> Option<Vec<u8>> {
  let labels: Vec<&str> = host.trim_end_matches('.').split('.').collect();
  if labels.is_empty()
    || labels
      .iter()
      .any(|label| label.is_empty() || label.len() > 63)
  {
    return None;
  }
  let total: usize = labels.iter().map(|label| label.len() + 1).sum::<usize>() + 1;
  if total > 255 {
    return None;
  }
  let id = (std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .subsec_nanos() as u16)
    ^ (std::process::id() as u16);
  let mut query = Vec::with_capacity(12 + total + 4);
  query.extend_from_slice(&id.to_be_bytes());
  query.extend_from_slice(&0x0100u16.to_be_bytes());
  query.extend_from_slice(&1u16.to_be_bytes());
  query.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
  for label in labels {
    query.push(label.len() as u8);
    query.extend_from_slice(label.as_bytes());
  }
  query.push(0);
  query.extend_from_slice(&query_type.to_be_bytes());
  query.extend_from_slice(&1u16.to_be_bytes());
  Some(query)
}

fn dns_answers(response: &[u8], query_id: u16, query_type: u16) -> Vec<IpAddr> {
  if response.len() < 12
    || u16::from_be_bytes([response[0], response[1]]) != query_id
    || u16::from_be_bytes([response[2], response[3]]) & 0x8000 == 0
  {
    return Vec::new();
  }
  let questions = u16::from_be_bytes([response[4], response[5]]) as usize;
  let answers = u16::from_be_bytes([response[6], response[7]]) as usize;
  let authorities = u16::from_be_bytes([response[8], response[9]]) as usize;
  let additionals = u16::from_be_bytes([response[10], response[11]]) as usize;
  let mut offset = 12;
  for _ in 0..questions {
    let Some(next) = skip_dns_name(response, offset) else {
      return Vec::new();
    };
    if next + 4 > response.len() {
      return Vec::new();
    }
    offset = next + 4;
  }
  let mut found = Vec::new();
  for index in 0..answers + authorities + additionals {
    let Some(next) = skip_dns_name(response, offset) else {
      return found;
    };
    if next + 10 > response.len() {
      return found;
    }
    let record_type = u16::from_be_bytes([response[next], response[next + 1]]);
    let class = u16::from_be_bytes([response[next + 2], response[next + 3]]);
    let length = u16::from_be_bytes([response[next + 8], response[next + 9]]) as usize;
    let data = next + 10;
    let end = data.saturating_add(length);
    if end > response.len() {
      return found;
    }
    if index < answers && class == 1 && record_type == query_type {
      match (record_type, length) {
        (1, 4) => found.push(IpAddr::V4(Ipv4Addr::new(
          response[data],
          response[data + 1],
          response[data + 2],
          response[data + 3],
        ))),
        (28, 16) => {
          let mut octets = [0u8; 16];
          octets.copy_from_slice(&response[data..end]);
          found.push(IpAddr::V6(Ipv6Addr::from(octets)));
        }
        _ => {}
      }
    }
    offset = end;
  }
  found
}

fn skip_dns_name(bytes: &[u8], mut offset: usize) -> Option<usize> {
  for _ in 0..128 {
    let length = *bytes.get(offset)?;
    if length & 0xc0 == 0xc0 {
      return (offset + 2 <= bytes.len()).then_some(offset + 2);
    }
    if length == 0 {
      return Some(offset + 1);
    }
    if length & 0xc0 != 0 || length > 63 {
      return None;
    }
    offset = offset.checked_add(length as usize + 1)?;
    if offset > bytes.len() {
      return None;
    }
  }
  None
}

fn origin_form(request: &[u8], header_end: usize) -> Vec<u8> {
  let Some(line_end) = request.get(..header_end).and_then(|head| {
    head
      .windows(2)
      .position(|window| window == b"\r\n")
      .map(|end| end + 2)
  }) else {
    return request.to_vec();
  };
  let line = &request[..line_end - 2];
  let mut parts = line.splitn(3, |byte| *byte == b' ');
  let Some(method) = parts.next() else {
    return request.to_vec();
  };
  let Some(target) = parts.next() else {
    return request.to_vec();
  };
  let Some(version) = parts.next() else {
    return request.to_vec();
  };
  let Some(scheme_end) = target.windows(3).position(|window| window == b"://") else {
    return request.to_vec();
  };
  let scheme = &target[..scheme_end];
  if scheme != b"http" && scheme != b"https" {
    return request.to_vec();
  }
  let authority_and_path = &target[scheme_end + 3..];
  let path_index = [
    authority_and_path.iter().position(|byte| *byte == b'/'),
    authority_and_path.iter().position(|byte| *byte == b'?'),
  ]
  .into_iter()
  .flatten()
  .min();
  let path = path_index
    .map(|index| &authority_and_path[index..])
    .unwrap_or(b"/");
  let mut normalized = Vec::with_capacity(request.len());
  normalized.extend_from_slice(method);
  normalized.push(b' ');
  normalized.extend_from_slice(path);
  normalized.push(b' ');
  normalized.extend_from_slice(version);
  normalized.extend_from_slice(&request[line_end..]);
  normalized
}

fn relay_bidirectionally(client: TcpStream, remote: TcpStream, stop: &Arc<AtomicBool>) {
  let Ok(client_to_remote) = client.try_clone() else {
    return;
  };
  let Ok(remote_to_client) = remote.try_clone() else {
    return;
  };
  let stop_a = Arc::clone(stop);
  let stop_b = Arc::clone(stop);
  let first = thread::spawn(move || copy_until_stopped(client_to_remote, remote, stop_a));
  let second = thread::spawn(move || copy_until_stopped(remote_to_client, client, stop_b));
  let _ = first.join();
  let _ = second.join();
}

fn copy_until_stopped(mut source: TcpStream, mut destination: TcpStream, stop: Arc<AtomicBool>) {
  let _ = source.set_read_timeout(Some(POLL));
  let _ = source.set_write_timeout(Some(POLL));
  let _ = destination.set_write_timeout(Some(POLL));
  let mut buffer = [0u8; 16 * 1024];
  loop {
    if stop.load(Ordering::Acquire) {
      let _ = destination.shutdown(Shutdown::Both);
      return;
    }
    match source.read(&mut buffer) {
      Ok(0) => {
        let _ = destination.shutdown(Shutdown::Write);
        return;
      }
      Ok(count) => {
        let mut written = 0;
        while written < count {
          match destination.write(&buffer[written..count]) {
            Ok(0) => return,
            Ok(next) => written += next,
            Err(error)
              if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
              ) =>
            {
              if stop.load(Ordering::Acquire) {
                let _ = destination.shutdown(Shutdown::Both);
                return;
              }
            }
            Err(_) => return,
          }
        }
      }
      Err(error)
        if matches!(
          error.kind(),
          io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) =>
      {
        continue;
      }
      Err(_) => return,
    }
  }
}

fn configured_upstream_proxy(target: &(String, u16)) -> io::Result<Option<UpstreamProxy>> {
  // Match ureq's environment precedence. The worker agent cannot use the
  // environment because its proxy is the localhost cancellation relay, so the
  // relay must preserve the operator's configured outbound route explicitly.
  // Honour the standard bypass list before selecting an upstream route.
  if std::env::var("NO_PROXY")
    .or_else(|_| std::env::var("no_proxy"))
    .ok()
    .is_some_and(|value| no_proxy_matches(&target.0, target.1, &value))
  {
    return Ok(None);
  }
  let mut unsupported = false;
  for name in [
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
  ] {
    let Ok(value) = std::env::var(name) else {
      continue;
    };
    if let Some((scheme, _)) = value.trim().split_once("://")
      && scheme != "http"
    {
      unsupported = true;
      continue;
    }
    if let Some(proxy) = parse_upstream_proxy(&value) {
      return Ok(Some(proxy));
    }
  }
  if unsupported {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "the configured proxy protocol is unsupported by the cancellable relay",
    ));
  }
  Ok(None)
}

fn no_proxy_matches(host: &str, port: u16, list: &str) -> bool {
  let host = host.trim_end_matches('.');
  list
    .split(|byte: char| byte == ',' || byte.is_ascii_whitespace())
    .filter_map(|entry| {
      let entry = entry.trim();
      if entry.is_empty() {
        return None;
      }
      if entry == "*" {
        return Some(true);
      }
      let (pattern, expected_port) = if entry.starts_with('[') {
        let close = entry.find(']')?;
        let pattern = &entry[1..close];
        let suffix = &entry[close + 1..];
        let expected_port = suffix
          .strip_prefix(':')
          .and_then(|value| value.parse::<u16>().ok());
        (pattern, expected_port)
      } else if entry.matches(':').count() == 1 {
        let (pattern, port) = entry.rsplit_once(':')?;
        (pattern, port.parse::<u16>().ok())
      } else {
        (entry, None)
      };
      if expected_port.is_some_and(|expected| expected != port) {
        return Some(false);
      }
      let pattern = pattern.trim_start_matches('.').trim_end_matches('.');
      if pattern.is_empty() {
        return Some(false);
      }
      let matches = host.eq_ignore_ascii_case(pattern)
        || host
          .strip_suffix(pattern)
          .is_some_and(|suffix| suffix.ends_with('.') && suffix.len() > 1);
      Some(matches)
    })
    .any(|matches| matches)
}

fn parse_upstream_proxy(value: &str) -> Option<UpstreamProxy> {
  let value = value.trim().trim_end_matches('/');
  let (scheme, authority) = value.split_once("://").unwrap_or(("http", value));
  if scheme != "http" || authority.is_empty() || authority.contains('/') || authority.contains('?')
  {
    return None;
  }
  let (credentials, authority) = authority
    .rsplit_once('@')
    .map_or((None, authority), |(credentials, authority)| {
      (Some(credentials), authority)
    });
  let (host, port) = parse_authority(authority, 80)?;
  let authorization = credentials.and_then(|credentials| {
    let (user, password) = credentials.split_once(':')?;
    if user.is_empty() {
      return None;
    }
    Some(format!(
      "Basic {}",
      base64_encode(format!("{user}:{password}").as_bytes())
    ))
  });
  Some(UpstreamProxy {
    host,
    port,
    authorization,
  })
}

fn parse_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
  if authority.starts_with('[') {
    let close = authority.find(']')?;
    let host = &authority[1..close];
    let suffix = authority.get(close + 1..)?;
    let port = if suffix.is_empty() {
      default_port
    } else {
      suffix.strip_prefix(':')?.parse().ok()?
    };
    return (!host.is_empty() && port > 0).then(|| (host.to_string(), port));
  }
  if authority.matches(':').count() > 1 {
    return None;
  }
  let (host, port) = authority
    .rsplit_once(':')
    .map_or((authority, default_port), |(host, port)| {
      (host, port.parse().unwrap_or(0))
    });
  if host.is_empty()
    || port == 0
    || authority
      .bytes()
      .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
  {
    return None;
  }
  Some((host.to_string(), port))
}

fn base64_encode(bytes: &[u8]) -> String {
  const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
  for chunk in bytes.chunks(3) {
    let first = chunk[0];
    let second = chunk.get(1).copied().unwrap_or(0);
    let third = chunk.get(2).copied().unwrap_or(0);
    output.push(TABLE[(first >> 2) as usize] as char);
    output.push(TABLE[((first & 0x03) << 4 | second >> 4) as usize] as char);
    output.push(if chunk.len() > 1 {
      TABLE[((second & 0x0f) << 2 | third >> 6) as usize] as char
    } else {
      '='
    });
    output.push(if chunk.len() > 2 {
      TABLE[(third & 0x3f) as usize] as char
    } else {
      '='
    });
  }
  output
}

fn target_from_url(url: &str) -> Option<(String, u16)> {
  let (scheme, rest) = url.split_once("://")?;
  let default_port = match scheme {
    "http" => 80,
    "https" => 443,
    _ => return None,
  };
  let authority = rest.split('/').next()?.split('?').next()?;
  if authority.is_empty() || authority.contains('@') {
    return None;
  }
  if let Some(host) = authority.strip_prefix('[') {
    let close = host.find(']')?;
    let name = &host[..close];
    let suffix = host.get(close + 1..)?;
    let port = if suffix.is_empty() {
      default_port
    } else {
      suffix.strip_prefix(':')?.parse().ok()?
    };
    return (!name.is_empty()
      && port > 0
      && !name
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()))
    .then(|| (name.to_string(), port));
  }
  if authority.matches(':').count() > 1 {
    return None;
  }
  if let Some((host, port)) = authority.rsplit_once(':') {
    let port = port.parse().ok()?;
    return (!host.is_empty()
      && port > 0
      && !host
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()))
    .then(|| (host.to_string(), port));
  }
  (!authority
    .bytes()
    .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control()))
  .then(|| (authority.to_string(), default_port))
}

#[cfg(test)]
mod tests {
  use super::{no_proxy_matches, target_from_url};

  #[test]
  fn rejects_ambiguous_authorities() {
    assert_eq!(target_from_url("https://[::1]garbage/v1"), None);
    assert_eq!(target_from_url("https://[::1]:bad/v1"), None);
    assert_eq!(target_from_url("https://2001:db8::1/v1"), None);
  }

  #[test]
  fn no_proxy_matches_domains_and_ports_without_overmatching() {
    assert!(no_proxy_matches("api.example.com", 443, "example.com"));
    assert!(no_proxy_matches(
      "api.example.com",
      8443,
      ".example.com:8443"
    ));
    assert!(!no_proxy_matches(
      "api.example.com",
      443,
      "example.com:8443"
    ));
    assert!(!no_proxy_matches("notexample.com", 443, "example.com"));
    assert!(no_proxy_matches("198.51.100.7", 443, "198.51.100.7"));
    assert!(no_proxy_matches("api.example.com", 443, "*"));
  }
}
