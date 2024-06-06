use std::{any::type_name, cmp::max, net::SocketAddr, time::Duration};

use serde::{de::DeserializeOwned, Serialize};
use stacked_errors::{Error, Result, StackableErr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{lookup_host, TcpListener, TcpStream},
    select,
    time::sleep,
};

use crate::{type_hash, wait_for_ok};

/// Waits for looking up a host's `SocketAddr` to be successful.
///
/// Note: it is possible for `lookup_host` to succeed, yet something like a
/// `TcpStream::connect` call immediately afterwards can still fail, so this
/// function by itself cannot be used as a barrier.
pub async fn wait_for_ok_lookup_host(
    num_retries: u64,
    delay: Duration,
    host: &str,
) -> Result<Vec<SocketAddr>> {
    async fn f(host: &str) -> Result<Vec<SocketAddr>> {
        match lookup_host(host).await {
            Ok(addrs) => Ok(addrs.into_iter().collect()),
            Err(e) => Err(e).stack_err(|| format!("wait_for_ok_lookup_host(.., host: {host})")),
        }
    }
    wait_for_ok(num_retries, delay, || f(host)).await
}

/// Waits for a tcp connection to be successful
pub async fn wait_for_ok_tcp_stream_connect(
    num_retries: u64,
    delay: Duration,
    socket_addr: SocketAddr,
) -> Result<TcpStream> {
    async fn f(socket_addr: SocketAddr) -> Result<TcpStream> {
        match TcpStream::connect(socket_addr).await {
            Ok(stream) => Ok(stream),
            Err(e) => Err(e).stack_err(|| {
                format!("wait_for_ok_tcp_stream_connect(.., socket_addr: {socket_addr})")
            }),
        }
    }
    wait_for_ok(num_retries, delay, || f(socket_addr)).await
}

// What we maybe need is a sequence of bijection statements macro which forms a
// single document for barriers and syncronization between different programs,
// maybe include ordinary code in it. It starts in the starting program, and at
// a DSL keyword it succinctly logically moves a tuple of things to the next
// program in parallel.

/// This is mainly intended for sending serializeable structs within
/// self-contained container networks
#[derive(Debug)]
pub struct NetMessenger {
    stream: TcpStream,
    // buffer whose capacity is kept around
    buf: Vec<u8>,
}

impl NetMessenger {
    /// Binds to and listens on `socket_addr`, and accepts a single connection
    /// to message with. Cancels the bind and returns a timeout error if
    /// `timeout` is reached first.
    pub async fn listen(host: &str, timeout: Duration) -> Result<Self> {
        let socket_addr = lookup_host(host)
            .await?
            .next()
            .stack_err(|| "NetMessenger::listen -> no socket addresses from lookup_host(host)")?;
        let listener = TcpListener::bind(socket_addr).await.stack()?;
        // we use the cancel safety of `tokio::net::TcpListener::accept
        select! {
            tmp = listener.accept() => {
                let (stream, _) = tmp.stack()?;
                Ok(Self {stream, buf: vec![]})
            }
            _ = sleep(timeout) => {
                Err(Error::timeout())
            }
        }
    }

    /// Connects to another `NetMessenger` that is being started with
    /// `listen`.
    pub async fn connect(num_retries: u64, delay: Duration, host: &str) -> Result<Self> {
        let socket_addrs = wait_for_ok_lookup_host(num_retries, delay, host)
            .await
            .stack()?;
        let socket_addr = *socket_addrs.first().stack_err(|| {
            "NetMessenger::connect -> wait_for_ok_lookup_host was ok but returned no socket \
             addresses"
        })?;
        let stream = wait_for_ok_tcp_stream_connect(num_retries, delay, socket_addr)
            .await
            .stack()?;
        Ok(Self {
            stream,
            buf: vec![],
        })
    }

    /// Sends `msg` to the connected party, waiting for a corresponding `recv`
    /// call.
    ///
    /// Note: You should always use the turbofish to specify `T`, because it is
    /// otherwise possible to get an unexpected type because of `Deref`
    /// coercion.
    ///
    /// Note: The hash of `std::any::type_name` is sent and compared to
    /// dynamically check if the correct `send` and `recv` pair are being used.
    /// This may break if the `send` and `recv` are sending from different
    /// binaries compiled by different compiler versions (but at least it is a
    /// false positive).
    pub async fn send<T: ?Sized + Serialize>(&mut self, msg: &T) -> Result<()> {
        loop {
            self.buf.clear();
            self.buf.resize(self.buf.capacity(), 0);
            match postcard::to_slice(msg, &mut self.buf) {
                Ok(_) => (),
                Err(postcard::Error::SerializeBufferFull) => {
                    // double the capacity
                    // TODO we need to add limits, maybe a settable option on the `NetMessage`
                    // struct
                    let current_cap = max(self.buf.capacity(), 1);
                    // reserve is based on `self.len() + additional` instead of
                    // `self.capacity() + additional`
                    let double = current_cap.wrapping_shl(1);
                    self.buf.reserve(double);
                    continue
                }
                Err(e) => {
                    return Err(Error::from_err(e))
                        .stack_err_locationless(|| "failed to serialize message")?
                }
            }
            break
        }
        // TODO handle timeouts
        let id = type_hash::<T>();
        if let Err(e) = self.stream.write_all(&id).await {
            return Err(Error::probably_not_root_cause()
                .add_kind_locationless(format!(
                    "NetMessenger::send::<{}>::() could not write_all, this may be because the \
                     other side was abruptly terminated",
                    type_name::<T>()
                ))
                .add_kind_locationless(e))
        }
        // later errors are probably real network errors
        self.stream
            .write_u64_le(u64::try_from(self.buf.len())?)
            .await
            .stack()?;
        self.stream.write_all(&self.buf).await.stack()?;
        self.stream.flush().await.stack()?;
        Ok(())
    }

    /// Waits for the connected party to `send` something with the same `T`.
    ///
    /// Note: If you don't directly assign the output to a binding with a
    /// specified type, you should always use the turbofish to specify `T`,
    /// because it is otherwise possible to get an unexpected type because
    /// of `Deref` coercion.
    pub async fn recv<T: ?Sized + DeserializeOwned>(&mut self) -> Result<T> {
        // TODO handle timeouts
        let expected_id = type_hash::<T>();
        let mut actual_id = [0u8; 16];
        if let Err(e) = self.stream.read_exact(&mut actual_id).await {
            return Err(Error::probably_not_root_cause()
                .add_kind_locationless(format!(
                    "NetMessenger::recv::<{}>::() could not read_exact, this may be because the \
                     other side was abruptly terminated",
                    type_name::<T>()
                ))
                .add_kind_locationless(e))
        }
        // later errors are probably real network errors
        if expected_id != actual_id {
            return Err(Error::from(format!(
                "NetMessenger::recv() -> incoming type did not match expected type ({})",
                type_name::<T>()
            )))
        }
        let data_len = usize::try_from(self.stream.read_u64_le().await.stack()?)?;
        if data_len > self.buf.len() {
            self.buf.resize_with(data_len, || 0);
        }
        self.stream
            .read_exact(&mut self.buf[0..data_len])
            .await
            .stack()?;
        postcard::from_bytes(&self.buf[0..data_len])
            .stack_err(|| "NetMessenger::recv() -> failed to deserialize message")
    }
}
