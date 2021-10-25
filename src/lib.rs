//! Manage a group of tasks on a runtime.
//!
//! Enables cancellation to be propagated across tasks, and ensures if an error
//! occurs that all sibling tasks in the group are cancelled too.
//!
//! # Examples
//!
//! Create an echo tcp server which processes incoming connections in a loop
//! without ever creating any dangling tasks:
//!
//! ```
//! use async_std::io;
//! use async_std::net::{TcpListener, TcpStream};
//! use async_std::prelude::*;
//! use async_std::task;
//!
//! async fn process(stream: TcpStream) -> io::Result<()> {
//!     println!("Accepted from: {}", stream.peer_addr()?);
//!
//!     let mut reader = stream.clone();
//!     let mut writer = stream;
//!     io::copy(&mut reader, &mut writer).await?;
//!
//!     Ok(())
//! }
//!
//! #[async_std::main]
//! fn main() -> io::Result<()> {
//!     let listener = TcpListener::bind("127.0.0.1:8080").await?;
//!     println!("Listening on {}", listener.local_addr()?);
//!
//!     let handle = async_task_group::group(|group| async move {
//!         let mut incoming = listener.incoming();
//!         while let Some(stream) = incoming.next().await {
//!             let stream = stream?;
//!             group.spawn(async move { process(stream).await });
//!         }
//!         Ok(())
//!     });
//!     handle.await?;
//!     Ok(())
//! }
//! ```

#![deny(missing_debug_implementations, nonstandard_style)]
#![warn(missing_docs, unreachable_pub)]

use async_channel::{self, Receiver, Sender};

use async_std::task::{self, JoinHandle};
use async_std::task::{Context, Poll};
use core::future::Future;
use core::pin::Pin;
use futures_core::Stream;

/// A TaskGroup is used to spawn a collection of tasks. The collection has two properties:
/// * if any task returns an error or panicks, all tasks are terminated.
/// * if the `JoinHandle` returned by `group` is dropped, all tasks are terminated.
pub struct TaskGroup<E> {
    sender: Sender<ChildHandle<E>>,
}

impl<E> std::fmt::Debug for TaskGroup<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskGroup").finish_non_exhaustive()
    }
}

/// Create a new instance.
pub fn group<E, Fut, F>(f: F) -> GroupJoinHandle<E>
where
    E: Send + 'static,
    F: FnOnce(TaskGroup<E>) -> Fut,
    Fut: Future<Output = Result<TaskGroup<E>, E>> + Send + 'static,
{
    let (sender, receiver) = async_channel::unbounded();
    let group = TaskGroup { sender };
    let join_handle = GroupJoinHandle::new(receiver);
    let fut = f(group.clone());
    group.spawn(async move {
        let _ = fut.await;
        Ok(())
    });
    join_handle
}

impl<E> TaskGroup<E>
where
    E: Send + 'static,
{
    /// Spawn a new task on the runtime.
    pub fn spawn<F>(&self, f: F)
    where
        F: Future<Output = Result<(), E>> + Send + 'static,
    {
        let join = task::spawn(f);
        self.sender
            .try_send(ChildHandle { handle: join })
            .expect("Sending a task to the channel failed");
    }

    /// Spawn a new local task on the runtime.
    pub fn spawn_local<F>(&self, f: F)
    where
        F: Future<Output = Result<(), E>> + 'static,
    {
        let join = task::spawn_local(f);
        self.sender
            .try_send(ChildHandle { handle: join })
            .expect("Sending a task to the channel failed");
    }

    /// Create a new builder.
    pub fn build(&self) -> GroupBuilder<'_, E> {
        GroupBuilder {
            task_group: self,
            builder: task::Builder::new(),
        }
    }

    /// Returns `true` if the task group has been shut down.
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    // Private clone method. This should not be public to guarantee no handle to
    // `TaskGroup` cannot outlive the closure in which it is granted. Once Rust
    // has async closures, we can pass `&TaskGroup` down the to closure.
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

/// Task builder that configures the settings of a new task
#[derive(Debug)]
pub struct GroupBuilder<'a, E> {
    task_group: &'a TaskGroup<E>,
    builder: task::Builder,
}

impl<'a, E> GroupBuilder<'a, E>
where
    E: Send + 'static,
{
    /// Configures the name of the task.
    pub fn name<A: AsRef<String>>(mut self, name: A) -> Self {
        self.builder = self.builder.name(name.as_ref().to_owned());
        self
    }

    /// Spawns a task with the configured settings.
    pub fn spawn<F>(self, future: F)
    where
        F: Future<Output = Result<(), E>> + Send + 'static,
    {
        let handle = self.builder.spawn(future).unwrap();
        self.task_group
            .sender
            .try_send(ChildHandle { handle })
            .expect("Sending a task to the channel failed");
    }

    ///Spawns a task locally with the configured settings.
    pub fn spawn_local<F>(self, future: F)
    where
        F: Future<Output = Result<(), E>> + 'static,
    {
        let handle = self.builder.local(future).unwrap();
        self.task_group
            .sender
            .try_send(ChildHandle { handle })
            .expect("Sending a task to the channel failed");
    }
}

#[derive(Debug)]
struct ChildHandle<E> {
    handle: JoinHandle<Result<(), E>>,
}

impl<E> ChildHandle<E> {
    // Pin projection. Since there is only this one required, avoid pulling in the proc macro.
    fn pin_join(self: Pin<&mut Self>) -> Pin<&mut JoinHandle<Result<(), E>>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.handle) }
    }
}

// As a consequence of this Drop impl, when a JoinHandle is dropped, all of its children will be
// canceled.
impl<E> Drop for ChildHandle<E> {
    fn drop(&mut self) {}
}

/// A JoinHandle is used to manage a collection of tasks. There are two
/// things you can do with it:
/// * JoinHandle impls Future, so you can poll or await on it. It will be
/// Ready when all tasks return Ok(()) and the associated `TaskGroup` is
/// dropped (so no more tasks can be created), or when any task panicks or
/// returns an Err(E).
/// * When a JoinHandle is dropped, all tasks it contains are canceled
/// (terminated). So, if you use a combinator like
/// `tokio::time::timeout(duration, task_manager).await`, all tasks will be
/// terminated if the timeout occurs.
#[derive(Debug)]
pub struct GroupJoinHandle<E> {
    channel: Option<Receiver<ChildHandle<E>>>,
    children: Vec<Pin<Box<ChildHandle<E>>>>,
}

impl<E> GroupJoinHandle<E> {
    fn new(channel: Receiver<ChildHandle<E>>) -> Self {
        Self {
            channel: Some(channel),
            children: Vec::new(),
        }
    }
}

impl<E> Future for GroupJoinHandle<E> {
    type Output = Result<(), E>;
    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        let mut s = self.as_mut();

        // If the channel is still open, take it out of s to satisfy the borrow checker.
        // We'll put it right back once we're done polling it.
        if let Some(mut channel) = s.channel.take() {
            // This loop processes each message in the channel until it is either empty
            // or closed.
            s.channel = loop {
                match unsafe { Pin::new_unchecked(&mut channel) }.poll_next(ctx) {
                    Poll::Pending => {
                        // No more messages, but channel still open
                        break Some(channel);
                    }
                    Poll::Ready(Some(new_child)) => {
                        // Put element from channel into the children
                        s.children.push(Box::pin(new_child));
                    }
                    Poll::Ready(None) => {
                        // Channel has closed and all messages have been recieved. No
                        // longer need channel.
                        break None;
                    }
                }
            };
        }

        // Need to mutate s after discovering error: store here temporarily
        let mut err = None;
        // Need to iterate through vec, possibly removing via swap_remove, so we cant use
        // a normal iterator:
        let mut child_ix = 0;
        while s.children.get(child_ix).is_some() {
            let child = s
                .children
                .get_mut(child_ix)
                .expect("precondition: child exists at index");
            match child.as_mut().pin_join().poll(ctx) {
                // Pending children get retained - move to next
                Poll::Pending => child_ix += 1,
                // Child returns successfully: remove it from children.
                // Then execute the loop body again with ix unchanged, because
                // last element was swapped into child_ix.
                Poll::Ready(Ok(())) => {
                    let _ = s.children.swap_remove(child_ix);
                }
                // Child returns with error: yield the error
                Poll::Ready(Err(error)) => {
                    err = Some(error);
                    break;
                }
            }
        }

        if let Some(err) = err {
            // Drop all children, and the channel reciever, current tasks are destroyed
            // and new tasks cannot be created:
            s.children.truncate(0);
            s.channel.take();
            // Return the error:
            Poll::Ready(Err(err))
        } else if s.children.is_empty() {
            if s.channel.is_none() {
                // Task manager is complete when there are no more children, and
                // no more channel to get more children:
                Poll::Ready(Ok(()))
            } else {
                // Channel is still pending, so we are not done:
                Poll::Pending
            }
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::anyhow;

    #[async_std::test]
    async fn no_task() {
        let handle = group(|group| async move { Ok::<_, ()>(group) });
        assert!(handle.await.is_ok());
    }

    #[async_std::test]
    async fn one_empty_task() {
        let handle = group(|group| async move {
            group.spawn(async move { Ok(()) });
            Ok::<_, ()>(group)
        });
        assert!(handle.await.is_ok());
    }

    #[async_std::test]
    async fn root_task_errors() {
        let handle = group(|group| async move {
            group.spawn(async { Err(anyhow!("idk!")) });
            Ok(group)
        });
        let res = handle.await;
        assert!(res.is_err());
        assert_eq!(format!("{:?}", res), "Err(idk!)");
    }
}
