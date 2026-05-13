//use std::io::{BufRead, BufReader, Read, Write};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use anyhow::{Context, Result};
use serde_json::Value;

const DOCKER_SOCKET: &str = "/var/run/docker.sock";

pub async fn load_archive(
    write_archive: impl FnOnce(&mut dyn AsyncWrite) -> Result<()>,
) -> Result<()> {
    let mut stream = UnixStream::connect(DOCKER_SOCKET)
        .await
        .with_context(|| format!("failed to connect to Docker socket {DOCKER_SOCKET}"))?;
    let request = "POST /images/load?quiet=0 HTTP/1.1\r\n\
         Host: docker\r\n\
         User-Agent: mepul/0.1.0\r\n\
         Content-Type: application/x-tar\r\n\
         Transfer-Encoding: chunked\r\n\
         Connection: close\r\n\
         \r\n";

    stream
        .write_all(request.as_bytes())
        .await
        .context("failed to send Docker load request")?;
    {
        let mut body = ChunkedWriter::new(&mut stream);
        write_archive(&mut body).context("failed to stream archive to Docker")?;
        body.finish()
            .await
            .context("failed to finish Docker load request body")?;
    }

    let mut reader = BufReader::new(stream);
    let response = read_response_head(&mut reader)?;
    if !response.status_success {
        let body = read_body(&mut reader, response.chunked)?;
        let message = String::from_utf8_lossy(&body);
        anyhow::bail!(
            "Docker load API failed: {} {}",
            response.status_line,
            message
        );
    }

    read_progress(&mut reader, response.chunked)?;
    Ok(())
}

struct ChunkedWriter<W> {
    inner: W,
    finished: bool,
}

impl<W: AsyncWrite + std::marker::Unpin> ChunkedWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            finished: false,
        }
    }

    async fn finish(&mut self) -> std::io::Result<()> {
        if !self.finished {
            self.inner.write_all(b"0\r\n\r\n").await?;
            self.finished = true;
        }
        self.inner.flush().await
    }
}

impl<W: AsyncWrite + std::marker::Unpin> AsyncWrite for ChunkedWriter<W> {
    // async fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    //     if buf.is_empty() {
    //         return Ok(0);
    //     }

    //     write!(self.inner, "{:x}\r\n", buf.len())?;
    //     self.inner.write_all(buf)?;
    //     self.inner.write_all(b"\r\n")?;
    //     Ok(buf.len())
    // }

    async fn write_chunk(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // 1. サイズを16進数文字列にして書き込む
        let header = format!("{:x}\r\n", buf.len());
        self.inner.write_all(header.as_bytes()).await?;

        // 2. 実際のデータを書き込む
        self.inner.write_all(buf).await?;

        // 3. チャンクの終端を書き込む
        self.inner.write_all(b"\r\n").await?;

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn read_response_head(reader: &mut BufReader<UnixStream>) -> Result<ResponseHead> {
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .context("failed to read Docker response status")?;
    let status_success = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .map(|code| (200..300).contains(&code))
        .unwrap_or(false);

    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("failed to read Docker response header")?;
        if line == "\r\n" || line.is_empty() {
            break;
        }

        let lower = line.to_ascii_lowercase();
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }

    Ok(ResponseHead {
        status_line: status_line.trim_end().to_string(),
        status_success,
        chunked,
    })
}

fn read_progress(reader: &mut BufReader<UnixStream>, chunked: bool) -> Result<()> {
    let mut pending = Vec::new();
    if chunked {
        read_chunked(reader, |bytes| {
            pending.extend_from_slice(bytes);
            print_complete_json_lines(&mut pending)
        })?;
    } else {
        reader
            .read_to_end(&mut pending)
            .context("failed to read Docker load response")?;
        print_complete_json_lines(&mut pending)?;
    }

    if !pending.is_empty() {
        print_progress_line(&pending)?;
    }
    Ok(())
}

fn read_body(reader: &mut BufReader<UnixStream>, chunked: bool) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    if chunked {
        read_chunked(reader, |bytes| {
            body.extend_from_slice(bytes);
            Ok(())
        })?;
    } else {
        reader
            .read_to_end(&mut body)
            .context("failed to read Docker response body")?;
    }
    Ok(body)
}

fn read_chunked(
    reader: &mut BufReader<UnixStream>,
    mut on_chunk: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    loop {
        let mut size_line = String::new();
        reader
            .read_line(&mut size_line)
            .context("failed to read Docker response chunk size")?;
        let size_text = size_line
            .trim()
            .split_once(';')
            .map(|(size, _)| size)
            .unwrap_or_else(|| size_line.trim());
        let size = usize::from_str_radix(size_text, 16)
            .with_context(|| format!("invalid Docker response chunk size: {size_text}"))?;
        if size == 0 {
            break;
        }

        let mut chunk = vec![0; size];
        reader
            .read_exact(&mut chunk)
            .context("failed to read Docker response chunk")?;
        on_chunk(&chunk)?;

        let mut crlf = [0; 2];
        reader
            .read_exact(&mut crlf)
            .context("failed to read Docker response chunk terminator")?;
    }
    Ok(())
}

fn print_complete_json_lines(buffer: &mut Vec<u8>) -> Result<()> {
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buffer.drain(..=pos).collect();
        print_progress_line(&line)?;
    }
    Ok(())
}

fn print_progress_line(line: &[u8]) -> Result<()> {
    let line = String::from_utf8_lossy(line).trim().to_string();
    if line.is_empty() {
        return Ok(());
    }

    let value: Value = serde_json::from_str(&line)
        .with_context(|| format!("failed to decode Docker progress line: {line}"))?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        anyhow::bail!("{error}");
    }
    if let Some(stream) = value.get("stream").and_then(Value::as_str) {
        print!("{stream}");
    } else if let Some(status) = value.get("status").and_then(Value::as_str) {
        println!("{status}");
    } else {
        println!("{line}");
    }

    Ok(())
}

struct ResponseHead {
    status_line: String,
    status_success: bool,
    chunked: bool,
}
