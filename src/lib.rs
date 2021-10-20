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
//!     let handle = task_group::group(|group| async move {
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

use async_global_executor::Task;
use async_std::task::{Context, Poll};
use core::future::Future;
use core::pin::Pin;
use futures_core::Stream;

/// A TaskGroup is used to spawn a collection of tasks. The collection has two properties:
/// * if any task returns an error or panicks, all tasks are terminated.
/// * if the `JoinHandle` returned by `group` is dropped, all tasks are terminated.
pub struct TaskGroup<E> {
    new_task: Sender<ChildHandle<E>>,
}

impl<E> std::fmt::Debug for TaskGroup<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskGroup").finish_non_exhaustive()
    }
}
// not the derived impl: E does not need to be Clone
impl<E> Clone for TaskGroup<E> {
    fn clone(&self) -> Self {
        Self {
            new_task: self.new_task.clone(),
        }
    }
}

/// Create a new instance.
pub fn group<E, Fut, F>(f: F) -> JoinHandle<E>
where
    E: Send + 'static,
    F: FnOnce(TaskGroup<E>) -> Fut,
    Fut: Future<Output = Result<(), E>> + Send + 'static,
{
    let (sender, receiver) = async_channel::unbounded();
    let group = TaskGroup { new_task: sender };
    let join_handle = JoinHandle::new(receiver);
    group.spawn(f(group.clone())); // FIXME move this to join handle rather than spawning it onto itself.
    join_handle
}

impl<E: Send + 'static> TaskGroup<E> {
    /// Spawn a new task on the runtime.
    pub fn spawn<F>(&self, f: F)
    where
        F: Future<Output = Result<(), E>> + Send + 'static,
    {
        let join = async_global_executor::spawn(f);
        self.new_task
            .try_send(ChildHandle { join })
            .expect("Sending a task to the channel failed");
    }

    /// Spawn a new local task on the runtime.
    pub fn spawn_local<F>(&self, f: F)
    where
        F: Future<Output = Result<(), E>> + 'static,
    {
        let join = async_global_executor::spawn_local(f);
        self.new_task
            .try_send(ChildHandle { join })
            .expect("Sending a task to the channel failed");
    }

    /// Returns `true` if the task group has been shut down.
    pub fn is_closed(&self) -> bool {
        self.new_task.is_closed()
    }
}

#[derive(Debug)]
struct ChildHandle<E> {
    join: Task<Result<(), E>>,
}

impl<E> ChildHandle<E> {
    // Pin projection. Since there is only this one required, avoid pulling in the proc macro.
    fn pin_join(self: Pin<&mut Self>) -> Pin<&mut Task<Result<(), E>>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.join) }
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
pub struct JoinHandle<E> {
    channel: Option<Receiver<ChildHandle<E>>>,
    children: Vec<Pin<Box<ChildHandle<E>>>>,
}

impl<E> JoinHandle<E> {
    fn new(channel: Receiver<ChildHandle<E>>) -> Self {
        Self {
            channel: Some(channel),
            children: Vec::new(),
        }
    }
}

impl<E> Future for JoinHandle<E> {
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
    use async_std::prelude::*;
    use async_std::sync::Mutex;
    use async_std::task::sleep;
    use std::sync::Arc;
    use std::time::Duration;

    #[async_std::test]
    async fn no_task() {
        let handle = group(|group| async move {
            drop(group); // Must drop the ability to spawn for the taskmanager to be finished
            Ok::<(), ()>(())
        });
        assert!(handle.await.is_ok());
    }

    #[async_std::test]
    async fn one_empty_task() {
        let handle = group(|group| async move {
            group.spawn(async move { Ok(()) });
            drop(group); // Must drop the ability to spawn for the taskmanager to be finished
            Ok::<(), ()>(())
        });
        assert!(handle.await.is_ok());
    }

    #[async_std::test]
    async fn empty_child() {
        let handle = group(|group| async move {
            group.clone().spawn(async move {
                group.spawn(async move { Ok(()) });
                Ok(())
            });
            Ok::<(), ()>(())
        });
        assert!(handle.await.is_ok());
    }

    #[async_std::test]
    async fn many_nested_children() {
        // Record a side-effect to demonstate that all of these children executed
        let log = Arc::new(Mutex::new(vec![0usize]));
        let l = log.clone();
        let handle = group(|group| async move {
            group.clone().spawn(async move {
                let log = log.clone();
                let group2 = group.clone();
                log.lock().await.push(1);
                group.spawn(async move {
                    let group3 = group2.clone();
                    log.lock().await.push(2);
                    group2.spawn(async move {
                        log.lock().await.push(3);
                        group3.spawn(async move {
                            log.lock().await.push(4);
                            Ok(())
                        });
                        Ok(())
                    });
                    Ok(())
                });
                Ok(())
            });
            Ok::<(), ()>(())
        });
        assert!(handle.await.is_ok());
        assert_eq!(*l.lock().await, vec![0usize, 1, 2, 3, 4]);
    }
    #[async_std::test]
    async fn many_nested_children_error() {
        // Record a side-effect to demonstate that all of these children executed
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec![]));
        let l = log.clone();

        let handle = group(|group| async move {
            let group2 = group.clone();
            group.spawn(async move {
                log.lock().await.push("in root");
                let group3 = group2.clone();
                group2.spawn(async move {
                    log.lock().await.push("in child");
                    let group4 = group3.clone();
                    group3.spawn(async move {
                        log.lock().await.push("in grandchild");
                        group4.spawn(async move {
                            log.lock().await.push("in great grandchild");
                            Err(anyhow!("sooner or later you get a failson"))
                        });
                        sleep(Duration::from_secs(1)).await;
                        // The great-grandchild returning error should terminate this task.
                        unreachable!("sleepy grandchild should never wake");
                    });
                    Ok(())
                });
                Ok(())
            });
            drop(group);
            Ok(())
        });
        assert_eq!(
            format!("{:?}", handle.await),
            "Err(sooner or later you get a failson)"
        );
        assert_eq!(
            *l.lock().await,
            vec![
                "in root",
                "in child",
                "in grandchild",
                "in great grandchild"
            ]
        );
    }
    #[async_std::test]
    async fn root_task_errors() {
        let handle = group(|group| async move {
            group.spawn(async { Err(anyhow!("idk!")) });
            Ok(())
        });
        let res = handle.await;
        assert!(res.is_err());
        assert_eq!(format!("{:?}", res), "Err(idk!)");
    }

    #[async_std::test]
    async fn child_task_errors() {
        let handle = group(|group| async move {
            group.clone().spawn(async move {
                group.spawn(async move { Err(anyhow!("whelp")) });
                Ok(())
            });
            Ok(())
        });
        let res = handle.await;
        assert!(res.is_err());
        assert_eq!(format!("{:?}", res), "Err(whelp)");
    }

    // #[async_std::test]
    // async fn root_task_panics() {
    //     let handle = group(|group| async move {
    //         group.spawn(async move { panic!("idk!") });
    //         Ok::<(), ()>(())
    //     });

    //     let res = handle.await;
    //     assert!(res.is_err());
    //     match res.err().unwrap() {
    //         e => panic!("wrong error variant! {:?}", e),
    //     }
    // }

    // #[async_std::test]
    // async fn child_task_panics() {
    //     let handle = group(|group| async move {
    //         let group2 = group.clone();
    //         group.spawn(async move {
    //             group2.spawn(async move { panic!("whelp") });
    //             Ok::<(), ()>(())
    //         });
    //         Ok::<(), ()>(())
    //     });

    //     let res = handle.await;
    //     assert!(res.is_err());
    //     match res.err().unwrap() {
    //         e => panic!("wrong error variant! {:?}", e),
    //     }
    // }

    #[async_std::test]
    async fn child_sleep_no_timeout() {
        // Record a side-effect to demonstate that all of these children executed
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec![]));
        let l = log.clone();
        let handle = group(|group| async move {
            let group2 = group.clone();
            group.spawn(async move {
                group2.spawn(async move {
                    log.lock().await.push("child gonna nap");
                    sleep(Duration::from_secs(1)).await; // 1 sec sleep, 2 sec timeout
                    log.lock().await.push("child woke up happy");
                    Ok(())
                });
                Ok(())
            });

            drop(group); // Not going to launch anymore tasks
            Ok::<(), ()>(())
        });

        let res = handle.timeout(Duration::from_secs(2)).await;
        assert!(res.is_ok(), "no timeout");
        assert!(res.unwrap().is_ok(), "returned successfully");
        assert_eq!(
            *l.lock().await,
            vec!["child gonna nap", "child woke up happy"]
        );
    }

    #[async_std::test]
    async fn child_sleep_timeout() {
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec![]));
        let l = log.clone();

        let handle = group(|group| async move {
            let group2 = group.clone();
            group.spawn(async move {
                group2.spawn(async move {
                    log.lock().await.push("child gonna nap");
                    sleep(Duration::from_secs(2)).await; // 2 sec sleep, 1 sec timeout
                    unreachable!("child should not wake from this nap");
                });
                Ok::<(), ()>(())
            });
            Ok(())
        });

        let res = handle.timeout(Duration::from_secs(1)).await;
        assert!(res.is_err(), "timed out");
        assert_eq!(*l.lock().await, vec!["child gonna nap"]);
    }
}
