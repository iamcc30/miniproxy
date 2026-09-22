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

/// 单条连接保留的 WS 文本总预算：超出后后续正文本体不再入库（仅记大小 + 截断标记）。
/// 之所以按「连接」而不是「单条消息」限制：像 `response.create` 这种一次几十 KB 的
/// JSON 逐帧都要看，但一条连接几十 MB 的日志也没有全存的必要。
pub const WS_TEXT_BUDGET: usize = 8 * 1024 * 1024;

/// 从 payload 中取可展示文本。
/// `budget` 为本次还能用的字节数；超出则按 UTF-8 字符边界安全截断。
/// 返回 `(文本, 实际占用字节数, 是否截断)`。
fn take_text(payload: &[u8], budget: usize) -> (String, usize, bool) {
    if payload.len() <= budget {
        return (String::from_utf8_lossy(payload).to_string(), payload.len(), false);
    }
    // 从 budget 处往回退到字符边界，避免把多字节字符切成半个
    let mut end = budget;
    while end > 0 && (payload[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    (String::from_utf8_lossy(&payload[..end]).to_string(), end, true)
}

fn make_message(dir: &'static str, opcode: u8, payload: &[u8], budget: usize) -> (WsMessage, usize) {
    let now = crate::capture::now_ms();
    let msg = |kind: &str, data: Option<String>, truncated: bool| WsMessage {
        dir,
        kind: kind.to_string(),
        size: payload.len(),
        data,
        truncated,
        ts: now,
    };
    match opcode {
        1 => {
            let (text, used, cut) = take_text(payload, budget);
            (msg("text", Some(text), cut), used)
        }
        2 => (msg("binary", None, false), 0),
        8 => {
            let code = if payload.len() >= 2 {
                format!("code={}", u16::from_be_bytes([payload[0], payload[1]]))
            } else {
                String::new()
            };
            let kind = format!("close{}", if code.is_empty() { "" } else { " " });
            (msg(&kind, if code.is_empty() { None } else { Some(code) }, false), 0)
        }
        9 => (msg("ping", None, false), 0),
        10 => (msg("pong", None, false), 0),
        _ => (msg(&format!("opcode-{}", opcode), None, false), 0),
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
                            let budget = WS_TEXT_BUDGET.saturating_sub(inner.ws_text_used);
                            let mut consumed = 0usize;
                            let mut merged = false;
                            if let Some(last) = inner.ws_messages.iter_mut().rev().find(|m| {
                                m.dir == dir && (m.kind == "text" || m.kind == "binary")
                            }) {
                                last.size += payload.len();
                                if last.kind == "text" {
                                    let (extra, used, cut) = take_text(&payload, budget);
                                    if !extra.is_empty() {
                                        let cur = last.data.take().unwrap_or_default();
                                        last.data = Some(format!("{}{}", cur, extra));
                                        consumed = used;
                                    }
                                    if cut {
                                        last.truncated = true;
                                    }
                                }
                                merged = true;
                            }
                            if merged {
                                inner.ws_text_used += consumed;
                                continue;
                            }
                        }
                        let budget = WS_TEXT_BUDGET.saturating_sub(inner.ws_text_used);
                        let (m, used) = make_message(dir, op, &payload, budget);
                        inner.ws_text_used += used;
                        inner.ws_messages.push(m);
                    }
                }
            }
        }
    }
}
