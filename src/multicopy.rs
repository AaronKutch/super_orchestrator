/*
use std::{future::Future, pin::Pin, task::{Context, Poll}};

use tokio::io::{AsyncRead, AsyncWrite};

// TODO there is a way to also put an async function in a box

type WriterType = Box<dyn Fn(&[u8]) -> crate::Result<usize>>;

struct MultiCopier<'a, R: AsyncRead> {
    reader: &'a mut R,
    writers: Vec<WriterType>
}

impl<R> Future for MultiCopier<'_, R>
where
    R: AsyncRead,
{
    type Output = crate::Result<u64>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<crate::Result<u64>> {
        let me = &mut *self;

        todo!()
    }
}

/// NOTE: you may have to use the syntax `Box::new(stuff) as Box<dyn Fn(&[u8]) -> crate::Result<usize>>`
pub async fn multicopy<'a, R: AsyncRead>(reader: &'a mut R, writers: Vec<WriterType>) -> crate::Result<u64> {
    Ok(0)
}
*/
