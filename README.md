
<h1 align="center">async-task-group</h1>
<div align="center">
  <strong>
    Manage groups of `async-std` tasks as a single unit.
  </strong>
</div>

<br />

<div align="center">
  <!-- Crates version -->
  <a href="https://crates.io/crates/async-task-group">
    <img src="https://img.shields.io/crates/v/async-task-group.svg?style=flat-square"
    alt="Crates.io version" />
  </a>
  <!-- Downloads -->
  <a href="https://crates.io/crates/async-task-group">
    <img src="https://img.shields.io/crates/d/async-task-group.svg?style=flat-square"
      alt="Download" />
  </a>
  <!-- docs.rs docs -->
  <a href="https://docs.rs/async-task-group">
    <img src="https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square"
      alt="docs.rs docs" />
  </a>
</div>

<div align="center">
  <h3>
    <a href="https://docs.rs/async-task-group">
      API Docs
    </a>
    <span> | </span>
    <a href="https://github.com/yoshuawuyts/async-task-group/releases">
      Releases
    </a>
    <span> | </span>
    <a href="https://github.com/yoshuawuyts/async-task-group/blob/master.github/CONTRIBUTING.md">
      Contributing
    </a>
  </h3>
</div>

# Credit

This codebase is based on the
[`task-group`](https://github.com/pchickey/task-group) project by Pat
Hickey.

## Examples

Create an echo tcp server which processes incoming connections in a loop
without ever creating any dangling tasks:

```rust
use async_std::io;
use async_std::net::{TcpListener, TcpStream};
use async_std::prelude::*;
use async_std::task;

async fn process(stream: TcpStream) -> io::Result<()> {
    println!("Accepted from: {}", stream.peer_addr()?);

    let mut reader = stream.clone();
    let mut writer = stream;
    io::copy(&mut reader, &mut writer).await?;

    Ok(())
}

#[async_std::main]
async fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("Listening on {}", listener.local_addr()?);

    let handle = async_task_group::group(|group| async move {
        let mut incoming = listener.incoming();
        while let Some(stream) = incoming.next().await {
            let stream = stream?;
            group.spawn(async move { process(stream).await });
        }
        Ok(group)
    });
    handle.await?;
    Ok(())
}
```

## Installation
```sh
$ cargo add async-task-group
```

## Safety
This crate uses ``#![deny(unsafe_code)]`` to ensure everything is implemented in
100% Safe Rust.

## Contributing
Want to join us? Check out our ["Contributing" guide][contributing] and take a
look at some of these issues:

- [Issues labeled "good first issue"][good-first-issue]
- [Issues labeled "help wanted"][help-wanted]

[contributing]: https://github.com/yoshuawuyts/async-task-group/blob/master.github/CONTRIBUTING.md
[good-first-issue]: https://github.com/yoshuawuyts/async-task-group/labels/good%20first%20issue
[help-wanted]: https://github.com/yoshuawuyts/async-task-group/labels/help%20wanted

## License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

<br/>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
</sub>
