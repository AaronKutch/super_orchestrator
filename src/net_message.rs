use std::{any::type_name, net::SocketAddr, time::Duration};

use musli::{en::Encode, mode::DefaultMode, Decode};
use musli_descriptive::Encoding;
use stacked_errors::{Error, MapAddError, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{lookup_host, TcpListener, TcpStream},
    select,
    time::sleep,
};

use crate::{type_hash, wait_for_ok};

const MUSLI_CONFIG: Encoding = musli_descriptive::encoding::DEFAULT;

/// Note: this is really only intended for self-contained Docker networks.
///
/// TODO what we need is a sequence of bijection statements macro which forms a
/// single document for barriers and syncronization between different programs,
/// maybe include ordinary code in it. It starts in the starting program, and at
/// a DSL keyword it succinctly logically moves a tuple of things to the next
/// program in parallel.
#[derive(Debug)]
pub struct NetMessenger {
    stream: TcpStream,
    // buffer whose capacity is kept around
    buf: Vec<u8>,
}

/// Waits for looking up a host's `SocketAddr` to be successful.
///
/// Note: it is possible for `lookup_host` to succeed, yet something like a
/// `TcpStream::connect` call immediately afterwards can still fail, so this
/// function by itself cannot be used as a barrier.
pub async fn wait_for_ok_lookup_host(
    num_retries: u64,
    delay: Duration,
    host: &str,
) -> Result<SocketAddr> {
    async fn f(host: &str) -> Result<SocketAddr> {
        match lookup_host(host).await {
            Ok(mut addrs) => {
                if let Some(addr) = addrs.next() {
                    Ok(addr)
                } else {
                    Err(Error::from("empty addrs"))
                }
            }
            Err(e) => Err(e).map_add_err(|| format!("wait_for_ok_lookup_host(.., host: {host})")),
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
            Err(e) => Err(e).map_add_err(|| {
                format!("wait_for_ok_tcp_stream_connect(.., socket_addr: {socket_addr})")
            }),
        }
    }
    wait_for_ok(num_retries, delay, || f(socket_addr)).await
}

impl NetMessenger {
    /// Binds to and listens on `socket_addr`, and accepts a single connection
    /// to message with. Cancels the bind and returns a timeout error if
    /// `timeout` is reached first.
    pub async fn listen_single_connect(host: &str, timeout: Duration) -> Result<Self> {
        let socket_addr = lookup_host(host)
            .await?
            .next()
            .map_add_err(|| "no socket addresses from lookup_host(host)")?;
        let listener = TcpListener::bind(socket_addr).await.map_add_err(|| ())?;

        //let tmp = listener.accept().await?;
        //let (stream, socket) = tmp;
        //Ok(Self {stream, socket})
        // we use the cancel safety of `tokio::net::TcpListener::accept
        select! {
            tmp = listener.accept() => {
                let (stream, _) = tmp.map_add_err(||())?;
                Ok(Self {stream, buf: vec![]})
            }
            _ = sleep(timeout) => {
                Err(Error::timeout())
            }
        }
    }

    /// Connects to another `NetMessenger` that is being started with
    /// `listen_single_connect`.
    pub async fn connect(num_retries: u64, delay: Duration, host: &str) -> Result<Self> {
        let socket_addr = wait_for_ok_lookup_host(num_retries, delay, host)
            .await
            .map_add_err(|| ())?;
        let stream = wait_for_ok_tcp_stream_connect(num_retries, delay, socket_addr)
            .await
            .map_add_err(|| ())?;
        Ok(Self {
            stream,
            buf: vec![],
        })
    }

    // TODO use like 8 bytes for the hash and also include some kind of string check
    // system (that can be configured to omit any expense)

    /// Note: You should always use the turbofish to specify `T`, because it is
    /// otherwise possible to get an unexpected type because of `&` coercion.
    ///
    /// Note: The hash of `std::any::type_name` is sent and compared to
    /// dynamically check if the correct `send` and `recv` pair are being used.
    /// This may break if the `send` and `recv` are sending from different
    /// binaries compiled by different compiler versions (but at least it is a
    /// false positive).
    pub async fn send<T: ?Sized + Encode<DefaultMode>>(&mut self, msg: &T) -> Result<()> {
        self.buf.clear();
        match MUSLI_CONFIG.encode(&mut self.buf, msg) {
            Ok(()) => (),
            Err(e) => return Err(Error::boxed(Box::new(e))),
        };
        // TODO handle timeouts
        let id = type_hash::<T>();
        if let Err(e) = self.stream.write_all(&id).await {
            return Err(Error::probably_not_root_cause()
                .add_err_no_location(format!(
                    "NetMessenger::send::<{}>::() could not write_all, this may be because the \
                     other side was abruptly terminated",
                    type_name::<T>()
                ))
                .add_err_no_location(e))
        }
        // later errors are probably real network errors
        self.stream
            .write_u64_le(u64::try_from(self.buf.len())?)
            .await
            .map_add_err(|| ())?;
        self.stream.write_all(&self.buf).await.map_add_err(|| ())?;
        self.stream.flush().await.map_add_err(|| ())?;
        Ok(())
    }

    /// Note: If you don't directly assign the output to a binding with a
    /// specified type, you should always use the turbofish to specify `T`,
    /// because it is otherwise possible to get an unexpected type because
    /// of `&` coercion.
    pub async fn recv<'de, T: ?Sized + Decode<'de, DefaultMode>>(&'de mut self) -> Result<T> {
        // TODO handle timeouts
        let expected_id = type_hash::<T>();
        let mut actual_id = [0u8; 16];
        if let Err(e) = self.stream.read_exact(&mut actual_id).await {
            return Err(Error::probably_not_root_cause()
                .add_err_no_location(format!(
                    "NetMessenger::recv::<{}>::() could not read_exact, this may be because the \
                     other side was abruptly terminated",
                    type_name::<T>()
                ))
                .add_err_no_location(e))
        }
        // later errors are probably real network errors
        if expected_id != actual_id {
            return Err(Error::from(format!(
                "NetMessenger::recv() -> incoming type did not match expected type ({})",
                type_name::<T>()
            )))
        }
        let data_len = usize::try_from(self.stream.read_u64_le().await.map_add_err(|| ())?)?;
        if data_len > self.buf.len() {
            self.buf.resize_with(data_len, || 0);
        }
        self.stream
            .read_exact(&mut self.buf[0..data_len])
            .await
            .map_add_err(|| ())?;
        match MUSLI_CONFIG.decode(&self.buf[0..data_len]) {
            Ok(o) => Ok(o),
            Err(e) => Err(Error::boxed(Box::new(e))),
        }
    }
}
