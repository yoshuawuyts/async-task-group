use async_channel::{self, Receiver, Sender};
use futures_core::Stream;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::task::JoinHandle as TokioJoinHandle;

/// A TaskGroup is used to spawn a collection of tasks. The collection has two properties:
/// * if any task returns an error or panicks, all tasks are terminated.
/// * if the `JoinHandle` returned by `group` is dropped, all tasks are terminated.
pub struct TaskGroup<E> {
    new_task: Sender<ChildHandle<E>>,
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
        let name = "blank".to_owned();
        let join = tokio::task::spawn(f);
        self.new_task
            .try_send(ChildHandle { name, join })
            .expect("Sending a task to the channel failed");
    }

    /// Spawn a new local task on the runtime.
    pub fn spawn_local<F>(&self, f: F)
    where
        F: Future<Output = Result<(), E>> + 'static,
    {
        let name = "blank".to_owned();
        let join = tokio::task::spawn_local(f);
        self.new_task
            .try_send(ChildHandle { name, join })
            .expect("Sending a task to the channel failed");
    }

    /// Returns `true` if the task group has been shut down.
    pub fn is_closed(&self) -> bool {
        self.new_task.is_closed()
    }
}

struct ChildHandle<E> {
    name: String,
    join: TokioJoinHandle<Result<(), E>>,
}

impl<E> ChildHandle<E> {
    // Pin projection. Since there is only this one required, avoid pulling in the proc macro.
    fn pin_join(self: Pin<&mut Self>) -> Pin<&mut TokioJoinHandle<Result<(), E>>> {
        unsafe { self.map_unchecked_mut(|s| &mut s.join) }
    }
    fn cancel(&mut self) {
        self.join.abort();
    }
}

// As a consequence of this Drop impl, when a JoinHandle is dropped, all of its children will be
// canceled.
impl<E> Drop for ChildHandle<E> {
    fn drop(&mut self) {
        self.cancel()
    }
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
    type Output = Result<(), RuntimeError<E>>;
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
                Poll::Ready(Ok(Ok(()))) => {
                    let _ = s.children.swap_remove(child_ix);
                }
                // Child returns with error: yield the error
                Poll::Ready(Ok(Err(error))) => {
                    err = Some(RuntimeError::Application {
                        name: child.name.clone(),
                        error,
                    });
                    break;
                }
                // Child join error: it either panicked or was canceled
                Poll::Ready(Err(e)) => {
                    err = Some(match e.try_into_panic() {
                        Ok(panic) => RuntimeError::Panic {
                            name: child.name.clone(),
                            panic,
                        },
                        Err(_) => unreachable!("impossible to cancel tasks in TaskGroup"),
                    });
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

#[derive(Debug)]
pub enum RuntimeError<E> {
    Panic {
        name: String,
        panic: Box<dyn Any + Send + 'static>,
    },
    Application {
        name: String,
        error: E,
    },
}
impl<E: std::fmt::Display + std::error::Error> std::error::Error for RuntimeError<E> {}
impl<E: std::fmt::Display> std::fmt::Display for RuntimeError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            RuntimeError::Panic { name, .. } => {
                write!(f, "Task `{}` panicked", name)
            }
            RuntimeError::Application { name, error } => {
                write!(f, "Task `{}` errored: {}", name, error)
            }
        }
    }
}

#[derive(Debug)]
pub enum SpawnError {
    GroupDied,
}
impl std::error::Error for SpawnError {}
impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            SpawnError::GroupDied => write!(f, "Task group died"),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::anyhow;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn no_task() {
        let handle = group(|group| async move {
            drop(group); // Must drop the ability to spawn for the taskmanager to be finished
            Ok::<(), ()>(())
        });
        assert!(handle.await.is_ok());
    }

    #[tokio::test]
    async fn one_empty_task() {
        let handle = group(|group| async move {
            group.spawn(async move { Ok(()) });
            drop(group); // Must drop the ability to spawn for the taskmanager to be finished
            Ok::<(), ()>(())
        });
        assert!(handle.await.is_ok());
    }

    #[tokio::test]
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

    #[tokio::test]
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
    #[tokio::test]
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
            "Err(Application { name: \"blank\", error: sooner or later you get a failson })"
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
    #[tokio::test]
    async fn root_task_errors() {
        let handle = group(|group| async move {
            group.spawn(async { Err(anyhow!("idk!")) });
            Ok(())
        });
        let res = handle.await;
        assert!(res.is_err());
        assert_eq!(
            format!("{:?}", res),
            "Err(Application { name: \"blank\", error: idk! })"
        );
    }

    #[tokio::test]
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
        assert_eq!(
            format!("{:?}", res),
            "Err(Application { name: \"blank\", error: whelp })"
        );
    }

    #[tokio::test]
    async fn root_task_panics() {
        let handle = group(|group| async move {
            group.spawn(async move { panic!("idk!") });
            Ok::<(), ()>(())
        });

        let res = handle.await;
        assert!(res.is_err());
        match res.err().unwrap() {
            RuntimeError::Panic { panic, .. } => {
                assert_eq!(*panic.downcast_ref::<&'static str>().unwrap(), "idk!");
            }
            e => panic!("wrong error variant! {:?}", e),
        }
    }

    #[tokio::test]
    async fn child_task_panics() {
        let handle = group(|group| async move {
            let group2 = group.clone();
            group.spawn(async move {
                group2.spawn(async move { panic!("whelp") });
                Ok::<(), ()>(())
            });
            Ok::<(), ()>(())
        });

        let res = handle.await;
        assert!(res.is_err());
        match res.err().unwrap() {
            RuntimeError::Panic { panic, .. } => {
                assert_eq!(*panic.downcast_ref::<&'static str>().unwrap(), "whelp");
            }
            e => panic!("wrong error variant! {:?}", e),
        }
    }

    #[tokio::test]
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

        let res = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(res.is_ok(), "no timeout");
        assert!(res.unwrap().is_ok(), "returned successfully");
        assert_eq!(
            *l.lock().await,
            vec!["child gonna nap", "child woke up happy"]
        );
    }

    #[tokio::test]
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

        let res = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(res.is_err(), "timed out");
        assert_eq!(*l.lock().await, vec!["child gonna nap"]);
    }
}
