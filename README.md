
# `task-group`

Create an echo tcp server which processes incoming connections in a loop
without ever creating any dangling tasks:

```rust
use async_std::prelude::*;
use async_std::io;
use async_std::net::{TcpListener, TcpStream};
use async_std::task;

async fn process(stream: TcpStream) -> io::Result<()> {
    println!("Accepted from: {}", stream.peer_addr()?);

    let mut reader = stream.clone();
    let mut writer = stream;
    io::copy(&mut reader, &mut writer).await?;

    Ok(())
}

#[async_std::main]
fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("Listening on {}", listener.local_addr()?);

    let handle = task_group::group(|group| async move {
        let mut incoming = listener.incoming();
        while let Some(stream) = incoming.next().await {
            let stream = stream?;
            group.spawn(async move { process(stream).await });
        }
        Ok(())
    });
    handle.await?;
    Ok(())
}
```


A `TaskGroup` is used to spawn a collection of tasks. The collection has two
properties:
* if any task returns an error or panicks, all tasks are terminated.
* if the `TaskManager` returned by `task_group::group` is dropped, all tasks are
terminated.


A `TaskManager` is used to manage a collection of tasks. There are two
things you can do with it:
* TaskManager impls Future, so you can poll or await on it. It will be
Ready when all tasks return Ok(()) and the associated `TaskGroup` is
dropped (so no more tasks can be created), or when any task panicks or
returns an Err(E).
* When a TaskManager is dropped, all tasks it contains are canceled
(terminated). So, if you use a combinator like
`tokio::time::timeout(duration, task_manager).await`, all tasks will be
terminated if the timeout occurs.

See `examples/` and the tests in `src/lib.rs` for more examples.
