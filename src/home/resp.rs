//! Bounded RESP2 codec. A cancelled or failed read/write must discard its connection.
use anyhow::{Result, bail, ensure};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_LINE_BYTES: usize = 64 * 1024;
pub const MAX_BULK_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_ARRAY_ITEMS: usize = 65_536;
pub const MAX_FRAME_NODES: usize = 131_072;
pub const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Simple(String),
    Error(String),
    Integer(i64),
    Bulk(Option<Vec<u8>>),
    Array(Option<Vec<Frame>>),
}

impl Frame {
    pub fn bytes(self) -> Result<Vec<u8>> {
        match self {
            Self::Bulk(Some(bytes)) => Ok(bytes),
            Self::Simple(text) => Ok(text.into_bytes()),
            Self::Error(_) => bail!("Home returned a RESP error"),
            Self::Bulk(None) => bail!("Home returned a null bulk string"),
            _ => bail!("expected a RESP bulk or simple string"),
        }
    }
}

fn charge(remaining: &mut usize, count: usize) -> Result<()> {
    *remaining = remaining
        .checked_sub(count)
        .ok_or_else(|| anyhow::anyhow!("RESP frame exceeds byte limit"))?;
    Ok(())
}

async fn line<R: AsyncBufRead + Unpin>(reader: &mut R, remaining: &mut usize) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        ensure!(!available.is_empty(), "unexpected EOF in RESP line");
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |n| n + 1);
        ensure!(
            line.len() + count <= MAX_LINE_BYTES + 2,
            "RESP line exceeds limit"
        );
        charge(remaining, count)?;
        let done = available[count - 1] == b'\n';
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if done {
            ensure!(line.ends_with(b"\r\n"), "invalid RESP line terminator");
            line.truncate(line.len() - 2);
            ensure!(!line.contains(&b'\r'), "invalid CR in RESP line");
            return Ok(line);
        }
    }
}

fn number(raw: &[u8]) -> Result<i64> {
    ensure!(!raw.is_empty(), "empty RESP integer");
    let digits = if raw[0] == b'-' { &raw[1..] } else { raw };
    ensure!(
        !digits.is_empty() && digits.iter().all(u8::is_ascii_digit),
        "invalid RESP integer"
    );
    Ok(std::str::from_utf8(raw)?.parse()?)
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Frame> {
    let mut remaining = MAX_FRAME_BYTES;
    let mut nodes = 0;
    // Store only received children, not capacity controlled by an untrusted length.
    let mut stack: Vec<(usize, Vec<Frame>)> = Vec::new();
    loop {
        nodes += 1;
        ensure!(nodes <= MAX_FRAME_NODES, "RESP frame exceeds node limit");
        charge(&mut remaining, 1)?;
        let prefix = reader.read_u8().await?;
        let raw = line(reader, &mut remaining).await?;
        let mut frame = match prefix {
            b'+' => Frame::Simple(String::from_utf8(raw)?),
            b'-' => Frame::Error(String::from_utf8(raw)?),
            b':' => Frame::Integer(number(&raw)?),
            b'$' => {
                let length = number(&raw)?;
                ensure!(length >= -1, "invalid RESP bulk length");
                if length == -1 {
                    Frame::Bulk(None)
                } else {
                    let length = usize::try_from(length)?;
                    ensure!(length <= MAX_BULK_BYTES, "RESP bulk exceeds limit");
                    charge(&mut remaining, length + 2)?;
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).await?;
                    let mut terminator = [0; 2];
                    reader.read_exact(&mut terminator).await?;
                    ensure!(terminator == *b"\r\n", "invalid RESP bulk terminator");
                    Frame::Bulk(Some(bytes))
                }
            }
            b'*' => {
                let length = number(&raw)?;
                ensure!(length >= -1, "invalid RESP array length");
                if length == -1 {
                    Frame::Array(None)
                } else {
                    ensure!(stack.len() < MAX_DEPTH, "RESP nesting exceeds limit");
                    let length = usize::try_from(length)?;
                    ensure!(length <= MAX_ARRAY_ITEMS, "RESP array exceeds limit");
                    if length != 0 {
                        stack.push((length, Vec::new()));
                        continue;
                    }
                    Frame::Array(Some(Vec::new()))
                }
            }
            _ => bail!("unsupported RESP2 type"),
        };
        loop {
            let Some((length, children)) = stack.last_mut() else {
                return Ok(frame);
            };
            children.push(frame);
            if children.len() != *length {
                break;
            }
            let (_, children) = stack.pop().expect("stack is nonempty");
            frame = Frame::Array(Some(children));
        }
    }
}

pub async fn write_command<W: AsyncWrite + Unpin>(writer: &mut W, args: &[Vec<u8>]) -> Result<()> {
    ensure!(
        !args.is_empty() && args.len() <= MAX_ARRAY_ITEMS,
        "invalid RESP command argument count"
    );
    let header = format!("*{}\r\n", args.len());
    let mut remaining = MAX_FRAME_BYTES;
    charge(&mut remaining, header.len())?;
    // Validate the entire command before sending anything.
    for arg in args {
        ensure!(
            arg.len() <= MAX_BULK_BYTES,
            "RESP argument exceeds bulk limit"
        );
        charge(
            &mut remaining,
            format!("${}\r\n", arg.len()).len() + arg.len() + 2,
        )?;
    }
    writer.write_all(header.as_bytes()).await?;
    for arg in args {
        writer
            .write_all(format!("${}\r\n", arg.len()).as_bytes())
            .await?;
        writer.write_all(arg).await?;
        writer.write_all(b"\r\n").await?;
    }
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{BufReader, duplex};

    #[tokio::test]
    async fn fragmented_nested_and_pipelined() {
        let (mut tx, rx) = duplex(1);
        let task = tokio::spawn(async move {
            for byte in
                b"*5\r\n+OK\r\n-ERR test\r\n:-42\r\n$3\r\na\0b\r\n*2\r\n$-1\r\n*-1\r\n+next\r\n"
            {
                tx.write_all(&[*byte]).await.unwrap();
            }
        });
        let mut reader = BufReader::with_capacity(1, rx);
        assert_eq!(
            read_frame(&mut reader).await.unwrap(),
            Frame::Array(Some(vec![
                Frame::Simple("OK".into()),
                Frame::Error("ERR test".into()),
                Frame::Integer(-42),
                Frame::Bulk(Some(b"a\0b".to_vec())),
                Frame::Array(Some(vec![Frame::Bulk(None), Frame::Array(None)])),
            ]))
        );
        assert_eq!(
            read_frame(&mut reader).await.unwrap().bytes().unwrap(),
            b"next"
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_malformed_and_oversized() {
        for raw in [
            b"+bad\n".as_slice(),
            b"$-2\r\n",
            b"*-3\r\n",
            b":+1\r\n",
            b":9223372036854775808\r\n",
            b"$2\r\nx",
            b"$1\r\nx!!",
            b"?x\r\n",
            b"*65537\r\n",
            b"$16777217\r\n",
        ] {
            assert!(
                read_frame(&mut BufReader::new(raw)).await.is_err(),
                "{raw:?}"
            );
        }
        let deep = format!("{}+x\r\n", "*1\r\n".repeat(MAX_DEPTH + 1));
        assert!(
            read_frame(&mut BufReader::new(deep.as_bytes()))
                .await
                .is_err()
        );
        let long = format!("+{}\r\n", "x".repeat(MAX_LINE_BYTES + 1));
        assert!(
            read_frame(&mut BufReader::new(long.as_bytes()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn binary_command_and_empty_bulk() {
        let mut output = Vec::new();
        write_command(&mut output, &[b"SET".to_vec(), vec![], b"\0\r\n".to_vec()])
            .await
            .unwrap();
        assert_eq!(output, b"*3\r\n$3\r\nSET\r\n$0\r\n\r\n$3\r\n\0\r\n\r\n");
        assert!(write_command(&mut output, &[]).await.is_err());
        assert_eq!(
            read_frame(&mut BufReader::new(b"$0\r\n\r\n".as_slice()))
                .await
                .unwrap()
                .bytes()
                .unwrap(),
            Vec::<u8>::new()
        );
        assert!(Frame::Error("secret".into()).bytes().is_err());
        assert!(Frame::Bulk(None).bytes().is_err());
    }
}
