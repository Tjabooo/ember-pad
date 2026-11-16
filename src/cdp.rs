//! Minimal Chrome DevTools Protocol client: find Discord's main page on the loopback debug port and
//! run `Runtime.evaluate` over a hand-rolled WebSocket (client frames masked, no Origin header so
//! Chromium's origin check lets us in).

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Conn {
    s: TcpStream,
    id: u64,
    mask: u32,
}

impl Conn {
    pub fn connect(port: u16) -> Result<Conn, String> {
        let addr = format!("127.0.0.1:{port}");
        let (_, body) = crate::http(&addr, "GET", "/json/list", None, Duration::from_millis(1500))
            .map_err(|_| "offline: start Discord with discord-wayland (debug port closed)".to_string())?;
        let list: Value = serde_json::from_str(&body).map_err(|e| format!("devtools list: {e}"))?;
        let url = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "page")
            .find(|t| t["url"].as_str().is_some_and(|u| u.contains("discord.com/") && !u.contains("overlay")))
            .and_then(|t| t["webSocketDebuggerUrl"].as_str())
            .ok_or("Discord's main window isn't up yet")?
            .to_owned();
        let path = url.split_once("://").map(|(_, r)| r).and_then(|r| r.find('/').map(|i| &r[i..])).ok_or("bad ws url")?;
        let mut s = TcpStream::connect(&addr).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(4))).ok();
        s.set_write_timeout(Some(Duration::from_secs(4))).ok();
        s.set_nodelay(true).ok();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: ZW1iZXItcGFkLWtleS0xMg==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).map_err(|e| format!("handshake: {e}"))?;
            head.push(b[0]);
            if head.len() > 8192 {
                return Err("handshake too long".into());
            }
        }
        let head = String::from_utf8_lossy(&head);
        if !head.starts_with("HTTP/1.1 101") {
            return Err(format!("handshake refused: {}", head.lines().next().unwrap_or("")));
        }
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1);
        Ok(Conn { s, id: 0, mask: seed | 1 })
    }

    pub fn eval(&mut self, expr: &str) -> Result<Value, String> {
        let r = self.call("Runtime.evaluate", json!({ "expression": expr, "returnByValue": true, "silent": true }))?;
        if let Some(ex) = r.get("exceptionDetails") {
            let text = ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("exception");
            return Err(format!("js: {}", text.lines().next().unwrap_or("")));
        }
        Ok(r["result"]["value"].clone())
    }

    /// Any DevTools method; returns its `result`.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.id += 1;
        let id = self.id;
        let msg = json!({ "id": id, "method": method, "params": params });
        self.send(0x1, msg.to_string().as_bytes())?;
        loop {
            let m = self.recv()?;
            let v: Value = serde_json::from_slice(&m).map_err(|e| e.to_string())?;
            if v["id"].as_u64() != Some(id) {
                continue; // an event or an old reply
            }
            if let Some(e) = v.get("error") {
                return Err(format!("devtools {method}: {e}"));
            }
            return Ok(v["result"].clone());
        }
    }

    fn next_mask(&mut self) -> [u8; 4] {
        self.mask ^= self.mask << 13;
        self.mask ^= self.mask >> 17;
        self.mask ^= self.mask << 5;
        self.mask.to_be_bytes()
    }

    fn send(&mut self, op: u8, data: &[u8]) -> Result<(), String> {
        let mut f = Vec::with_capacity(data.len() + 14);
        f.push(0x80 | op);
        match data.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n < 65536 => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask = self.next_mask();
        f.extend_from_slice(&mask);
        f.extend(data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.s.write_all(&f).map_err(|e| e.to_string())
    }

    fn recv(&mut self) -> Result<Vec<u8>, String> {
        let mut msg = Vec::new();
        loop {
            let mut h = [0u8; 2];
            self.s.read_exact(&mut h).map_err(|e| e.to_string())?;
            let (fin, op) = (h[0] & 0x80 != 0, h[0] & 0x0f);
            let masked = h[1] & 0x80 != 0;
            let mut len = (h[1] & 0x7f) as u64;
            if len == 126 {
                let mut b = [0u8; 2];
                self.s.read_exact(&mut b).map_err(|e| e.to_string())?;
                len = u16::from_be_bytes(b) as u64;
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.s.read_exact(&mut b).map_err(|e| e.to_string())?;
                len = u64::from_be_bytes(b);
            }
            if len > 64 << 20 {
                return Err("frame too large".into());
            }
            let mut mask = [0u8; 4];
            if masked {
                self.s.read_exact(&mut mask).map_err(|e| e.to_string())?;
            }
            let mut data = vec![0u8; len as usize];
            self.s.read_exact(&mut data).map_err(|e| e.to_string())?;
            if masked {
                data.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
            }
            match op {
                0x8 => return Err("devtools closed the connection".into()),
                0x9 => self.send(0xA, &data)?,
                0xA => {}
                _ => {
                    msg.extend_from_slice(&data);
                    if fin {
                        return Ok(msg);
                    }
                }
            }
        }
    }
}
