use std::{
    fmt,
    fmt::Debug,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll, Waker},
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock},
};

struct BufInner {
    buf: Box<[u8]>,
    // how many bytes of the buffer there are to read
    fill: usize,
}

struct Buf {
    // in order to use `OwnedRwLockWriteGuard` the RwLock needs to be in its own `Arc`
    inner_buf: Arc<RwLock<BufInner>>,
    reads_left: Arc<AtomicUsize>,
}

impl Buf {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner_buf: Arc::new(RwLock::new(BufInner {
                buf: vec![0u8; capacity].into_boxed_slice(),
                fill: 0,
            })),
            reads_left: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn arc_clone(&self) -> Self {
        Self {
            inner_buf: Arc::clone(&self.inner_buf),
            reads_left: Arc::clone(&self.reads_left),
        }
    }

    pub async fn write_owned(&self) -> OwnedRwLockWriteGuard<BufInner> {
        Arc::clone(&self.inner_buf).write_owned().await
    }

    pub async fn read_owned(&self) -> OwnedRwLockReadGuard<BufInner> {
        Arc::clone(&self.inner_buf).read_owned().await
    }
}

struct RwStream {
    // current buf that we are trying to read/write
    current_buf: bool,
    // the idea is that the writer and readers alternate between reading and writing to opposite
    // buffers
    buf0: Buf,
    buf1: Buf,
    // wakers
    write_waker: Arc<RwLock<Option<Waker>>>,
}

impl RwStream {
    pub fn arc_clone(&self, current_buf: bool) -> Self {
        Self {
            current_buf,
            buf0: self.buf0.arc_clone(),
            buf1: self.buf1.arc_clone(),
            write_waker: Arc::clone(&self.write_waker),
        }
    }

    pub fn current_buf(&self) -> &Buf {
        if self.current_buf {
            &self.buf0
        } else {
            &self.buf1
        }
    }

    /// Note that this gets the number of readers left on the opposite buffer
    pub fn get_readers_left(&self) -> usize {
        if self.current_buf {
            // TODO relax the orderings
            self.buf0.reads_left.load(Ordering::SeqCst)
        } else {
            self.buf1.reads_left.load(Ordering::SeqCst)
        }
    }

    pub async fn write_owned(&self) -> OwnedRwLockWriteGuard<BufInner> {
        if self.current_buf {
            self.buf1.write_owned().await
        } else {
            self.buf0.write_owned().await
        }
    }

    pub async fn read_owned(&self) -> OwnedRwLockReadGuard<BufInner> {
        if self.current_buf {
            self.buf1.read_owned().await
        } else {
            self.buf0.read_owned().await
        }
    }
}

pub struct RwStreamWriter {
    rw_stream: RwStream,
    // At least one lock must be held at all times, this acts as a barrier to the readers. The
    // second part of the trick is that the writer checks t
    lock0: Option<OwnedRwLockWriteGuard<BufInner>>,
    lock1: Option<OwnedRwLockWriteGuard<BufInner>>,
}

pub struct RwStreamReader {
    rw_stream: RwStream,
    // intermediate lock for in case a single `poll_read` call doesn't get everything at once, we
    // need to hold something inbetween calls to prevent the writer from running ahead. The `usize`
    // determines how much of the buffer we have read ourselves so far
    lock: Option<(usize, OwnedRwLockReadGuard<BufInner>)>,
}

impl AsyncWrite for RwStreamWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        if let Some(lock) = self.lock0.as_mut() {
            let fill = lock.fill;
            let our_buf = &mut lock.buf[fill..];
            let our_buf_len = our_buf.len();
            let bytes_written = if buf.len() > our_buf.len() {
                our_buf.copy_from_slice(&buf[0..our_buf_len]);
                our_buf_len
            } else {
                lock.buf[..buf.len()].copy_from_slice(buf);
                buf.len()
            };
            lock.fill += bytes_written;
            // note: _only_ when no readers are left can we proceed to alternate, otherwise
            // writer -> reader barrier invariant is broken
            if self.rw_stream.get_readers_left() == 0 {
                //
            }
            Poll::Ready(Ok(bytes_written))
        } else {
            Poll::Pending
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        todo!()
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        todo!()
    }
}

impl AsyncRead for RwStreamReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Some((ref mut bytes_read_so_far, lock)) = self.lock.as_mut() {
            let filled = lock.fill;
            let our_buf = &lock.buf[*bytes_read_so_far..filled];
            let remaining = buf.remaining();
            if remaining < our_buf.len() {
                buf.put_slice(&our_buf[0..remaining]);
                *bytes_read_so_far += remaining;
            } else {
                buf.put_slice(our_buf);
                // unlock because we are done with this round
                let _ = self.lock.take().unwrap();
                // get a read lock on the writer waker before decrementing. this prevents a race
                // condition where the writer sees the `reads_left` is nonzero and moves to set
                // up the waker, and the last reader finishes after checking for the waker
                self.rw_stream.write_waker.read().await;
                let prev_reads = self
                    .rw_stream
                    .current_buf()
                    .reads_left
                    .fetch_sub(1, Ordering::SeqCst);
                if prev_reads == 1 {
                    // see if the writer needs a wake up
                }
            }
            Poll::Ready(Ok(()))
        } else {
            // TODO
            Poll::Pending
        }
    }
}

/// Allows a simultaneous writer and multiple readers of a stream, the readers
/// all see the same data from the writer. This uses mass buffering of `u8`s
/// unlike broadcast channels and SPMC solutions.
pub async fn rw_stream_with_capacity(
    num_readers: usize,
    capacity: usize,
) -> (RwStreamWriter, Vec<RwStreamReader>) {
    let buf0 = Buf::new(capacity);
    let buf1 = Buf::new(capacity);
    let mut writer = RwStreamWriter {
        rw_stream: RwStream {
            current_buf: false,
            buf0,
            buf1,
            write_waker: Arc::new(RwLock::new(None)),
        },
        lock0: None,
        lock1: None,
    };
    let mut readers = vec![];
    for _ in 0..num_readers {
        let reader = RwStreamReader {
            rw_stream: writer.rw_stream.arc_clone(false),
            lock: None,
        };
        readers.push(reader);
    }
    writer.lock0 = Some(writer.rw_stream.write_owned().await);
    (writer, readers)
}

/// [rw_stream_with_capacity] with a default 8kb capacity
pub async fn rw_stream(num_readers: usize) -> (RwStreamWriter, Vec<RwStreamReader>) {
    rw_stream_with_capacity(num_readers, 1 << 13).await
}

impl RwStreamWriter {}

impl Debug for RwStreamReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwStreamReader").finish()
    }
}

impl Debug for RwStreamWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwStreamWriter").finish()
    }
}

#[tokio::test]
async fn test() {
    use tokio::sync::Mutex;
    let num_readers = 10;
    let (writer, mut readers) = rw_stream(num_readers).await;
    let targets = vec![Arc::new(Mutex::new(Vec::<u8>::new())); num_readers];
    let mut handles = vec![];
    for i in 0..num_readers {
        let target = Arc::clone(&targets[i]);
        let reader = readers.pop().unwrap();
        handles.push(tokio::task::spawn(async {
            dbg!(target);
            dbg!(reader);
        }))
    }
    let to_write: Vec<u8> = vec![];
    handles.push(tokio::task::spawn(async {
        dbg!(writer);
    }));
    for handle in handles {
        handle.await.unwrap();
    }
    for target in targets {
        assert_eq!(to_write, *target.lock().await);
    }
}
