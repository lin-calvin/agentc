//! A minimal WebSocket server (subset of RFC 6455).
//!
//! Only what we need: handshake, receive text/binary, send text/binary, ping/pong/close.
//! No tokio, no tungstenite — the surface is tiny and hand-writing it is cheaper than
//! pulling in an async runtime.

use std::io::{self, Read, Write};
use std::net::TcpStream;

pub enum Msg {
    Text(String),
    Binary(Vec<u8>),
    Close,
    Ping(Vec<u8>),
    Pong,
}

pub struct WsConn {
    stream: TcpStream,
}

impl WsConn {
    /// Handshake. `request_head` is the already-read HTTP head (including \r\n\r\n).
    pub fn accept(mut stream: TcpStream, request_head: &str) -> io::Result<Self> {
        let key = request_head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.eq_ignore_ascii_case("sec-websocket-key") {
                    Some(v.trim().to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no websocket key"))?;

        let accept = accept_key(&key);
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        stream.write_all(resp.as_bytes())?;
        stream.flush()?;
        stream.set_nodelay(true).ok();
        Ok(Self { stream })
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.stream.read_exact(buf)
    }

    /// Read one complete message, reassembling fragments.
    pub fn read(&mut self) -> io::Result<Msg> {
        let mut acc: Option<(u8, Vec<u8>)> = None;
        loop {
            let mut h = [0u8; 2];
            self.read_exact(&mut h)?;
            let fin = h[0] & 0x80 != 0;
            let opcode = h[0] & 0x0f;
            let masked = h[1] & 0x80 != 0;
            let len7 = (h[1] & 0x7f) as u64;
            let len = match len7 {
                126 => {
                    let mut b = [0u8; 2];
                    self.read_exact(&mut b)?;
                    u16::from_be_bytes(b) as u64
                }
                127 => {
                    let mut b = [0u8; 8];
                    self.read_exact(&mut b)?;
                    u64::from_be_bytes(b)
                }
                n => n,
            };
            if len > 32 << 20 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too big"));
            }
            let mut mask = [0u8; 4];
            if masked {
                self.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len as usize];
            if len > 0 {
                self.read_exact(&mut payload)?;
            }
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i & 3];
                }
            }

            match opcode {
                0x8 => return Ok(Msg::Close),
                0x9 => return Ok(Msg::Ping(payload)),
                0xa => return Ok(Msg::Pong),
                0x1 | 0x2 => {
                    if fin {
                        return Ok(if opcode == 0x1 {
                            Msg::Text(String::from_utf8_lossy(&payload).to_string())
                        } else {
                            Msg::Binary(payload)
                        });
                    }
                    acc = Some((opcode, payload));
                }
                0x0 => {
                    if let Some((op, mut buf)) = acc.take() {
                        buf.extend_from_slice(&payload);
                        if fin {
                            return Ok(if op == 0x1 {
                                Msg::Text(String::from_utf8_lossy(&buf).to_string())
                            } else {
                                Msg::Binary(buf)
                            });
                        }
                        acc = Some((op, buf));
                    }
                }
                _ => {}
            }
        }
    }

    fn send_frame(&mut self, opcode: u8, data: &[u8]) -> io::Result<()> {
        let mut head = Vec::with_capacity(10);
        head.push(0x80 | opcode);
        let n = data.len();
        if n < 126 {
            head.push(n as u8);
        } else if n <= 0xffff {
            head.push(126);
            head.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            head.push(127);
            head.extend_from_slice(&(n as u64).to_be_bytes());
        }
        self.stream.write_all(&head)?;
        self.stream.write_all(data)?;
        self.stream.flush()
    }

    pub fn send_text(&mut self, s: &str) -> io::Result<()> {
        self.send_frame(0x1, s.as_bytes())
    }

    pub fn send_binary(&mut self, b: &[u8]) -> io::Result<()> {
        self.send_frame(0x2, b)
    }

    pub fn pong(&mut self, p: &[u8]) -> io::Result<()> {
        self.send_frame(0xa, p)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

    pub fn close(&mut self) {
        let _ = self.send_frame(0x8, &[]);
    }
}

// ---------------------------------------------------------------- handshake

fn accept_key(key: &str) -> String {
    let mut input = Vec::with_capacity(key.len() + 36);
    input.extend_from_slice(key.as_bytes());
    input.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64(sha1(&input).as_slice())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// SHA-1, needed only for the handshake. Performance does not matter here.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bitlen = (msg.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in msg.chunks_exact(64) {
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn sha1_known() {
        // test vector from RFC 3174
        let h = super::sha1(b"abc");
        assert_eq!(
            h,
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
                0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
            ]
        );
    }
}
