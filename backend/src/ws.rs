//! WebSocket 帧解析器（RFC 6455）+ 双向泵（转发同时捕获消息）。

use crate::capture::{Entry, WsMessage};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Default)]
pub struct FrameParser {
    buf: Vec<u8>,
}

/// 解析出的事件：(opcode, payload)。opcode: 1=text 2=binary 0=cont 8=close 9=ping 10=pong
pub type FrameEvent = (u8, Vec<u8>);

impl FrameParser {
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<FrameEvent>) {
        self.buf.extend_from_slice(data);
        loop {
            if self.buf.len() < 2 {
                break;
            }
            let b0 = self.buf[0];
            let b1 = self.buf[1];
            let opcode = b0 & 0x0f;
            let masked = b1 & 0x80 != 0;
            let mut len = (b1 & 0x7f) as u64;
            let mut off = 2usize;
            if len == 126 {
                if self.buf.len() < 4 {
                    break;
                }
                len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as u64;
                off = 4;
            } else if len == 127 {
                if self.buf.len() < 10 {
                    break;
                }
                len = u64::from_be_bytes(self.buf[2..10].try_into().unwrap());
                off = 10;
            }
            let mask_len = if masked { 4 } else { 0 };
            let total = off + mask_len + len as usize;
            if self.buf.len() < total {
                // 防御：异常大的帧直接丢弃缓冲
                if len > 64 * 1024 * 1024 {
                    self.buf.clear();
                    out.push((opcode, Vec::new()));
                }
                break;
            }
            let mut payload = self.buf[off + mask_len..total].to_vec();
            // 客户端 -> 服务端的帧带掩码，展示前需 XOR 还原
            if masked {
                let mask = &self.buf[off..off + 4];
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            self.buf.drain(..total);
            out.push((opcode, payload));
        }
    }
}

fn make_message(dir: &'static str, opcode: u8, payload: &[u8]) -> WsMessage {
    let now = crate::capture::now_ms();
    match opcode {
        1 => WsMessage {
            dir,
            kind: "text".into(),
            size: payload.len(),
            data: Some(String::from_utf8_lossy(payload).chars().take(4096).collect()),
            ts: now,
        },
        2 => WsMessage {
            dir,
            kind: "binary".into(),
            size: payload.len(),
            data: None,
            ts: now,
        },
        8 => {
            let code = if payload.len() >= 2 {
                format!("code={}", u16::from_be_bytes([payload[0], payload[1]]))
            } else {
                String::new()
            };
            WsMessage {
                dir,
                kind: format!("close{}", if code.is_empty() { "" } else { " " }),
                size: payload.len(),
                data: if code.is_empty() { None } else { Some(code) },
                ts: now,
            }
        }
        9 => WsMessage { dir, kind: "ping".into(), size: payload.len(), data: None, ts: now },
        10 => WsMessage { dir, kind: "pong".into(), size: payload.len(), data: None, ts: now },
        _ => WsMessage {
            dir,
            kind: format!("opcode-{}", opcode),
            size: payload.len(),
            data: None,
            ts: now,
        },
    }
}

/// 双向泵：一边转发一边解析 WebSocket 帧，记录消息到 entry。
pub async fn splice_ws<A, B>(a: A, b: B, entry: Arc<Entry>)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (a_r, a_w) = tokio::io::split(a);
    let (b_r, b_w) = tokio::io::split(b);

    let c2s = pump(a_r, b_w, "c2s", entry.clone());
    let s2c = pump(b_r, a_w, "s2c", entry.clone());
    tokio::select! {
        _ = c2s => {},
        _ = s2c => {},
    }
    {
        let mut inner = entry.inner.lock().unwrap();
        inner.done = true;
        inner.ws_closed = true;
    }
}

async fn pump<R, W>(mut r: R, mut w: W, dir: &'static str, entry: Arc<Entry>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut parser = FrameParser::default();
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if w.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                let mut events = Vec::new();
                parser.feed(&buf[..n], &mut events);
                if !events.is_empty() {
                    let mut inner = entry.inner.lock().unwrap();
                    if dir == "c2s" {
                        inner.bytes_up += n as u64;
                    } else {
                        inner.bytes_down += n as u64;
                    }
                    for (op, payload) in events {
                        // 续帧：尝试合并到上一条同方向消息
                        if op == 0 {
                            if let Some(last) = inner.ws_messages.iter_mut().rev().find(|m| m.dir == dir && (m.kind == "text" || m.kind == "binary")) {
                                last.size += payload.len();
                                if last.kind == "text" && last.data.is_some() {
                                    let extra = String::from_utf8_lossy(&payload);
                                    let cur = last.data.clone().unwrap_or_default();
                                    if cur.len() < 4096 {
                                        last.data = Some(format!("{}{}", cur, extra).chars().take(4096).collect());
                                    }
                                }
                                continue;
                            }
                        }
                        inner.ws_messages.push(make_message(dir, op, &payload));
                    }
                }
            }
        }
    }
}
