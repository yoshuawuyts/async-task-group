use anyhow::{anyhow, Error};
use tokio::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let handle = task_group::group(|group| async move {
        println!("hello dogs!");

        group.spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            println!("Gussie goes and sucks on a blanket");
            Ok(())
        });

        group.spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            println!("Willa wants to play outside");
            tokio::time::sleep(Duration::from_secs(1)).await;
            if true {
                println!("Willa is upset and about to do something naughty");
                Err(anyhow!("willa is chewing on the blinds"))
            } else {
                Ok(())
            }
        });

        group.spawn(async move {
            for _ in 1..4usize {
                tokio::time::sleep(Duration::from_millis(500)).await;
                println!("Sparky wants to go out too");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            println!("Sparky is taking a nap");
            Ok(())
        });

        Ok(())
    });

    match tokio::time::timeout_at(deadline, handle).await {
        Ok(Ok(())) => {
            println!("dogs have not defeated me");
            Ok(())
        }
        Ok(Err(error)) => Err(error.context(format!("task died"))),
        Err(_) => Err(anyhow!("timeout")),
    }
}
