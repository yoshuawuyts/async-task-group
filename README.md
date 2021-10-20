
# `task-group`

A small crate for managing groups of tokio tasks.

```rust

let (task_group, task_manager): (TaskGroup<Error>, TaskManager<_>) = task_group::group();

task_group.clone().spawn("a task", async move {
    task_group.spawn("b task", async move {
        /* all kinds of things */
        Ok(())
    }).await.expect("spawned b");
    Ok(())
}).await.expect("spawned a");

task_manager.await.expect("everyone successful");

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
