use std::io::SeekFrom;
use std::path::Path;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio_watch::{Result, WatchedFile};

async fn write_test_file(path: impl AsRef<Path>, append: bool) -> Result<()> {
    let mut file = OpenOptions::new()
        .read(false)
        .write(true)
        .create(true)
        .truncate(!append)
        .open(path)
        .await?;
    if append {
        file.seek(SeekFrom::End(0)).await?;
    }
    for i in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        file.write_all(format!("Line {}\n", i).as_bytes()).await?;
        file.sync_data().await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    {
        //make sure the file exists and is cleared.
        let _ = OpenOptions::new()
            .read(false)
            .write(true)
            .create(true)
            .truncate(true)
            .open("watched.txt")
            .await?;
    }
    let file = WatchedFile::new("watched.txt").await?;

    tokio::spawn(async move {
        //write to a new blank file
        write_test_file("watched.txt", false).await?;
        //append to it
        write_test_file("watched.txt", true).await?;
        //and recreate it
        write_test_file("watched.txt", false).await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let handle = tokio::spawn(async move {
        let mut file = BufReader::new(file).lines();
        while let Some(line) = file.next_line().await.unwrap() {
            println!("{}", line);
        }
    });
    tokio::signal::ctrl_c().await?;
    tokio::fs::remove_file("watched.txt").await?;
    handle.await?;
    Ok(())
}
