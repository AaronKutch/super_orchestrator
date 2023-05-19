use std::time::Duration;

use musli::{en::Encode, mode::DefaultMode, Decode};
use musli_descriptive::Encoding;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{lookup_host, TcpListener, TcpStream},
    select,
    time::sleep,
};

use crate::{Error, MapAddError, Result};

const MUSLI_CONFIG: Encoding = musli_descriptive::encoding::DEFAULT;

#[derive(Debug)]
pub struct NetMessenger {
    stream: TcpStream,
    // buffer whose capacity is kept around
    buf: Vec<u8>,
}

impl NetMessenger {
    pub async fn connect(host: &str) -> Result<Self> {
        let socket_addr = lookup_host(host)
            .await?
            .next()
            .map_add_err(|| "no socket addresses from lookup_host")?;
        let stream = TcpStream::connect(socket_addr).await.map_add_err(|| ())?;
        Ok(Self {
            stream,
            buf: vec![],
        })
    }

    /// Binds to and listens on `socket_addr`, and accepts a single connection
    /// to message with. Cancels the bind and returns a timeout error if
    /// `timeout` is reached first.
    pub async fn listen_single_connect(host: &str, timeout: Duration) -> Result<Self> {
        let socket_addr = lookup_host(host)
            .await?
            .next()
            .map_add_err(|| "no socket addresses from lookup_host")?;
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

    pub async fn send<T: ?Sized + Encode<DefaultMode>>(&mut self, msg: &T) -> Result<()> {
        dbg!(self.buf.len());
        match MUSLI_CONFIG.encode(&mut self.buf, msg) {
            Ok(()) => (),
            Err(e) => return Err(Error::boxed(Box::new(e))),
        };
        dbg!(self.buf.len());
        // TODO handle timeouts
        self.stream
            .write_u64_le(u64::try_from(self.buf.len())?)
            .await
            .map_add_err(|| ())?;
        self.stream.write_all(&self.buf).await.map_add_err(|| ())?;
        self.stream.flush().await.map_add_err(|| ())?;
        Ok(())
    }

    pub async fn recv<'de, T: ?Sized + Decode<'de, DefaultMode>>(&'de mut self) -> Result<T> {
        // TODO handle timeouts
        let data_len = usize::try_from(self.stream.read_u64_le().await?)?;
        dbg!(data_len);
        if data_len > self.buf.len() {
            self.buf.resize_with(data_len, || 0);
        }
        self.stream.read_exact(&mut self.buf[0..data_len]).await?;
        match MUSLI_CONFIG.decode(&self.buf[0..data_len]) {
            Ok(o) => Ok(o),
            Err(e) => Err(Error::boxed(Box::new(e))),
        }
    }
}
