//! Database-backed Home KV only. No local cache, fallback, or mutation retries.
//! Wire contract: Home internal/respserver/kv/{handlers.go,register.go}.
use super::{client::HomeClient, resp::Frame};
use anyhow::{Context, Result, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SetMode {
    #[default]
    Always,
    Nx,
    Xx,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Seconds(u64),
    Milliseconds(u64),
}
impl Expiry {
    fn append(self, args: &mut Vec<Vec<u8>>) -> Result<()> {
        let (unit, count, multiplier) = match self {
            Self::Seconds(n) => (b"EX".as_slice(), n, 1_000_000_000),
            Self::Milliseconds(n) => (b"PX".as_slice(), n, 1_000_000),
        };
        // Home multiplies the signed count into a Go time.Duration (nanoseconds).
        ensure!(
            count > 0 && count <= i64::MAX as u64 / multiplier,
            "invalid Home TTL"
        );
        args.extend([unit.to_vec(), count.to_string().into_bytes()]);
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SetOptions {
    pub mode: SetMode,
    /// None removes any prior TTL on a successful write, matching Home SET.
    pub expiry: Option<Expiry>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    Missing,
    Persistent,
    Seconds(u64),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected<'a> {
    Absent,
    Value(&'a [u8]),
}

/// Borrowed client facade. All operations use the acknowledged Home lifetime.
pub struct Kv<'a> {
    home: &'a HomeClient,
}
impl<'a> Kv<'a> {
    pub fn new(home: &'a HomeClient) -> Self {
        Self { home }
    }
    /// MGET avoids Home's reserved GET keys and JSON request dispatch.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut values = self.mget(&[key]).await?;
        Ok(values.pop().expect("validated one-element MGET"))
    }
    pub async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.get(key)
            .await?
            .map(|bytes| decode_value(&bytes))
            .transpose()
    }
    pub async fn set(&self, key: &str, value: &[u8], options: SetOptions) -> Result<bool> {
        decode_set(
            self.home.command(set_command(key, value, options)?).await?,
            options.mode,
        )
    }
    pub async fn set_json<T: Serialize + ?Sized>(
        &self,
        key: &str,
        value: &T,
        options: SetOptions,
    ) -> Result<bool> {
        self.set(key, &serde_json::to_vec(value)?, options).await
    }
    /// Integer SETNX reply, unlike SET NX's OK/null reply.
    pub async fn set_nx(&self, key: &str, value: &[u8]) -> Result<bool> {
        let args = vec![b"SETNX".to_vec(), key_bytes(key)?, value.to_vec()];
        decode_bool(self.home.command(args).await?)
    }
    /// Byte-exact comparison. None TTL removes prior expiration on success.
    pub async fn compare_and_swap(
        &self,
        key: &str,
        expected: Expected<'_>,
        value: &[u8],
        ttl_ms: Option<u64>,
    ) -> Result<bool> {
        decode_bool(
            self.home
                .command(cas_command(key, expected, value, ttl_ms)?)
                .await?,
        )
    }
    /// Ordered results, preserving missing keys, empty values, and duplicate keys.
    pub async fn mget(&self, keys: &[&str]) -> Result<Vec<Option<Vec<u8>>>> {
        decode_mget(
            self.home.command(keys_command("MGET", keys)?).await?,
            keys.len(),
        )
    }
    pub async fn mget_json<T: DeserializeOwned>(&self, keys: &[&str]) -> Result<Vec<Option<T>>> {
        self.mget(keys)
            .await?
            .into_iter()
            .map(|value| value.map(|bytes| decode_value(&bytes)).transpose())
            .collect()
    }
    /// Home uses the last value for duplicate keys and removes prior TTLs.
    pub async fn mset(&self, pairs: &[(&str, &[u8])]) -> Result<()> {
        decode_ok(self.home.command(mset_command(pairs)?).await?)
    }
    pub async fn incr_by(&self, key: &str, delta: i64) -> Result<i64> {
        decode_integer(
            self.home
                .command(vec![
                    b"INCRBY".to_vec(),
                    key_bytes(key)?,
                    delta.to_string().into_bytes(),
                ])
                .await?,
        )
    }
    pub async fn incr(&self, key: &str) -> Result<i64> {
        self.incr_by(key, 1).await
    }
    pub async fn del(&self, keys: &[&str]) -> Result<u64> {
        let count = decode_integer(self.home.command(keys_command("DEL", keys)?).await?)?;
        ensure!(
            count >= 0 && count as u64 <= keys.len() as u64,
            "invalid Home DEL count"
        );
        Ok(count as u64)
    }
    /// Home accepts strictly positive EXPIRE seconds, not Redis's deletion shorthand.
    pub async fn expire(&self, key: &str, seconds: u64) -> Result<bool> {
        let mut expiry = Vec::new();
        Expiry::Seconds(seconds).append(&mut expiry)?;
        decode_bool(
            self.home
                .command(vec![b"EXPIRE".to_vec(), key_bytes(key)?, expiry.remove(1)])
                .await?,
        )
    }
    pub async fn ttl(&self, key: &str) -> Result<Ttl> {
        decode_ttl(
            self.home
                .command(vec![b"TTL".to_vec(), key_bytes(key)?])
                .await?,
        )
    }
}

fn key_bytes(key: &str) -> Result<Vec<u8>> {
    ensure!(!key.trim().is_empty(), "empty Home KV key");
    // Home trims keys itself; require canonical keys rather than silently aliasing them.
    ensure!(key == key.trim(), "Home KV key has surrounding whitespace");
    Ok(key.as_bytes().to_vec())
}
fn keys_command(command: &str, keys: &[&str]) -> Result<Vec<Vec<u8>>> {
    ensure!(!keys.is_empty(), "Home KV command needs at least one key");
    let mut args = vec![command.as_bytes().to_vec()];
    for key in keys {
        args.push(key_bytes(key)?);
    }
    Ok(args)
}
fn set_command(key: &str, value: &[u8], options: SetOptions) -> Result<Vec<Vec<u8>>> {
    let mut args = vec![b"SET".to_vec(), key_bytes(key)?, value.to_vec()];
    match options.mode {
        SetMode::Always => {}
        SetMode::Nx => args.push(b"NX".to_vec()),
        SetMode::Xx => args.push(b"XX".to_vec()),
    }
    if let Some(expiry) = options.expiry {
        expiry.append(&mut args)?;
    }
    Ok(args)
}
fn cas_command(
    key: &str,
    expected: Expected<'_>,
    value: &[u8],
    ttl_ms: Option<u64>,
) -> Result<Vec<Vec<u8>>> {
    let (exists, old) = match expected {
        Expected::Absent => (b"0".as_slice(), b"".as_slice()),
        Expected::Value(bytes) => (b"1".as_slice(), bytes),
    };
    let mut args = vec![
        b"CAS".to_vec(),
        key_bytes(key)?,
        exists.to_vec(),
        old.to_vec(),
        value.to_vec(),
    ];
    if let Some(ms) = ttl_ms {
        Expiry::Milliseconds(ms).append(&mut args)?;
    }
    Ok(args)
}
fn mset_command(pairs: &[(&str, &[u8])]) -> Result<Vec<Vec<u8>>> {
    ensure!(!pairs.is_empty(), "Home MSET needs at least one pair");
    let mut args = vec![b"MSET".to_vec()];
    for (key, value) in pairs {
        args.extend([key_bytes(key)?, value.to_vec()]);
    }
    Ok(args)
}
fn decode_value<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    // Do not include stored content in diagnostics.
    serde_json::from_slice(bytes).context("invalid typed Home KV value")
}
fn decode_integer(frame: Frame) -> Result<i64> {
    match frame {
        Frame::Integer(n) => Ok(n),
        _ => bail!("expected Home KV integer reply"),
    }
}
fn decode_bool(frame: Frame) -> Result<bool> {
    match decode_integer(frame)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => bail!("invalid Home KV boolean reply"),
    }
}
fn decode_ok(frame: Frame) -> Result<()> {
    ensure!(
        matches!(frame, Frame::Simple(ref s) if s == "OK"),
        "expected Home KV OK reply"
    );
    Ok(())
}
fn decode_set(frame: Frame, mode: SetMode) -> Result<bool> {
    if matches!(frame, Frame::Bulk(None)) {
        ensure!(
            mode != SetMode::Always,
            "unexpected unconditional SET rejection"
        );
        return Ok(false);
    }
    decode_ok(frame)?;
    Ok(true)
}
fn decode_mget(frame: Frame, count: usize) -> Result<Vec<Option<Vec<u8>>>> {
    let Frame::Array(Some(values)) = frame else {
        bail!("expected Home MGET array")
    };
    ensure!(values.len() == count, "invalid Home MGET result count");
    values
        .into_iter()
        .map(|frame| match frame {
            Frame::Bulk(value) => Ok(value),
            _ => bail!("expected Home MGET bulk element"),
        })
        .collect()
}
fn decode_ttl(frame: Frame) -> Result<Ttl> {
    match decode_integer(frame)? {
        -2 => Ok(Ttl::Missing),
        -1 => Ok(Ttl::Persistent),
        n if n >= 0 => Ok(Ttl::Seconds(n as u64)),
        _ => bail!("invalid Home TTL reply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn set_options_binary_and_ttl_bounds() {
        let args = set_command(
            "key",
            b"\0\r\n",
            SetOptions {
                mode: SetMode::Nx,
                expiry: Some(Expiry::Milliseconds(1500)),
            },
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                b"SET".to_vec(),
                b"key".to_vec(),
                b"\0\r\n".to_vec(),
                b"NX".to_vec(),
                b"PX".to_vec(),
                b"1500".to_vec()
            ]
        );
        assert_eq!(
            set_command(
                "k",
                b"",
                SetOptions {
                    mode: SetMode::Xx,
                    expiry: Some(Expiry::Seconds(2))
                }
            )
            .unwrap()[3..],
            [b"XX".to_vec(), b"EX".to_vec(), b"2".to_vec()]
        );
        for expiry in [
            Expiry::Seconds(0),
            Expiry::Milliseconds(0),
            Expiry::Seconds(u64::MAX),
            Expiry::Milliseconds(i64::MAX as u64),
        ] {
            assert!(expiry.append(&mut Vec::new()).is_err());
        }
        assert!(
            Expiry::Milliseconds(i64::MAX as u64 / 1_000_000)
                .append(&mut Vec::new())
                .is_ok()
        );
        assert!(decode_set(Frame::Bulk(None), SetMode::Nx).is_ok_and(|v| !v));
        assert!(decode_set(Frame::Bulk(None), SetMode::Always).is_err());
        assert!(decode_set(Frame::Integer(1), SetMode::Nx).is_err());
        assert!(decode_set(Frame::Simple("OK".into()), SetMode::Always).unwrap());
    }
    #[test]
    fn cas_missing_is_not_present_empty() {
        let absent = cas_command("k", Expected::Absent, b"new", None).unwrap();
        let empty = cas_command("k", Expected::Value(b""), b"new", Some(1)).unwrap();
        assert_eq!(absent[2], b"0");
        assert_eq!(absent[3], b"");
        assert_eq!(empty[2], b"1");
        assert_eq!(empty[5..], [b"PX".to_vec(), b"1".to_vec()]);
        let binary = cas_command("k", Expected::Value(b"\0"), b"\xff", None).unwrap();
        assert_eq!(binary[3], b"\0");
        assert_eq!(binary[4], b"\xff");
    }
    #[test]
    fn mget_preserves_order_missing_empty_and_duplicates() {
        assert_eq!(
            keys_command("MGET", &["config", "{}", "config"]).unwrap()[1..],
            [b"config".to_vec(), b"{}".to_vec(), b"config".to_vec()]
        );
        let values = decode_mget(
            Frame::Array(Some(vec![
                Frame::Bulk(None),
                Frame::Bulk(Some(vec![])),
                Frame::Bulk(Some(vec![255])),
            ])),
            3,
        )
        .unwrap();
        assert_eq!(values, vec![None, Some(vec![]), Some(vec![255])]);
        assert!(decode_mget(Frame::Array(Some(vec![])), 1).is_err());
        assert!(decode_mget(Frame::Array(None), 1).is_err());
        assert!(decode_mget(Frame::Array(Some(vec![Frame::Simple("x".into())])), 1).is_err());
        assert!(keys_command("DEL", &[]).is_err());
        assert!(mset_command(&[]).is_err());
        assert_eq!(
            mset_command(&[("k", b"a"), ("k", b"b")]).unwrap()[1..],
            [b"k".to_vec(), b"a".to_vec(), b"k".to_vec(), b"b".to_vec()]
        );
        for key in ["", " ", " k", "k "] {
            assert!(key_bytes(key).is_err());
        }
    }
    #[test]
    fn reply_types_and_typed_json() {
        assert_eq!(decode_ttl(Frame::Integer(-2)).unwrap(), Ttl::Missing);
        assert_eq!(decode_ttl(Frame::Integer(-1)).unwrap(), Ttl::Persistent);
        assert_eq!(decode_ttl(Frame::Integer(0)).unwrap(), Ttl::Seconds(0));
        assert!(decode_ttl(Frame::Integer(-3)).is_err());
        assert!(decode_bool(Frame::Integer(2)).is_err());
        assert!(!decode_bool(Frame::Integer(0)).unwrap());
        assert!(decode_bool(Frame::Integer(1)).unwrap());
        for frame in [
            Frame::Error("secret".into()),
            Frame::Bulk(None),
            Frame::Simple("1".into()),
        ] {
            assert!(decode_integer(frame).is_err());
        }
        assert!(decode_ok(Frame::Bulk(Some(b"OK".to_vec()))).is_err());
        #[derive(Debug, PartialEq, serde::Deserialize)]
        struct Stored {
            count: u64,
        }
        assert_eq!(
            decode_value::<Stored>(br#"{"count":2}"#).unwrap(),
            Stored { count: 2 }
        );
        assert!(decode_value::<Stored>(br#"{"count":"2"}"#).is_err());
        assert!(decode_value::<Stored>(b"null").is_err());
        assert!(decode_value::<Stored>(b"\xff").is_err());
    }
}
