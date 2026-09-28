//! 纯 TCP 隧道记录：转发字节流，统计上下行流量并捕获首包十六进制预览。

use crate::capture::Entry;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn splice_tcp<A, B>(a: A, b: B, entry: Arc<Entry>)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_r, mut a_w) = tokio::io::split(a);
    let (mut b_r, mut b_w) = tokio::io::split(b);

    let up = pump(&mut a_r, &mut b_w, true, entry.clone());
    let down = pump(&mut b_r, &mut a_w, false, entry.clone());
    tokio::select! {
        _ = up => {},
        _ = down => {},
    }
    entry.inner.lock().unwrap().finish(None);
}

async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    r: &mut R,
    w: &mut W,
    up: bool,
    entry: Arc<Entry>,
) {
    let mut buf = vec![0u8; 32 * 1024];
    let mut first = true;
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if w.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                let mut inner = entry.inner.lock().unwrap();
                if up {
                    inner.bytes_up += n as u64;
                } else {
                    inner.bytes_down += n as u64;
                }
                if first {
                    inner.tcp_hex = Some(crate::util::hex_preview(&buf[..n.min(256)]));
                    first = false;
                }
            }
        }
    }
    let _ = w.shutdown().await;
}
