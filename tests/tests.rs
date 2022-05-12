use std::io::SeekFrom;
use std::path::Path;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio_watch::{Result, WatchedFile};

async fn write_test_file(path: impl AsRef<Path>, append: bool, n_lines: usize) -> Result<()> {
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
    for i in 0..n_lines {
        file.write_all(format!("Line {}\n", i).as_bytes()).await?;
        file.sync_data().await?;
    }
    Ok(())
}

async fn touch(path: impl AsRef<Path>) -> Result<()> {
    //make sure the file exists and is cleared.
    OpenOptions::new()
        .read(false)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await?;
    Ok(())
}

#[tokio::test]
async fn simple_read() -> Result<()> {
    touch("simple_read").await?;
    tokio::spawn(async move {
        write_test_file("simple_read", false, 5).await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        tokio::fs::remove_file("simple_read").await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let file = WatchedFile::new("simple_read").await?;
    let handle = tokio::spawn(async move {
        let mut file = BufReader::new(file).lines();
        let mut lines = Vec::new();
        while let Some(line) = file.next_line().await.unwrap() {
            lines.push(line);
        }
        return lines;
    });
    let lines = handle.await?;
    let expected = vec!["Line 0", "Line 1", "Line 2", "Line 3", "Line 4"];
    assert_eq!(lines, expected);
    Ok(())
}

#[tokio::test]
async fn truncate() -> Result<()> {
    touch("truncate").await?;
    tokio::spawn(async move {
        write_test_file("truncate", false, 3).await?;
        write_test_file("truncate", false, 2).await?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        tokio::fs::remove_file("truncate").await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let file = WatchedFile::new("truncate").await?;
    let handle = tokio::spawn(async move {
        let mut file = BufReader::new(file).lines();
        let mut lines = Vec::new();
        while let Some(line) = file.next_line().await.unwrap() {
            lines.push(line);
        }
        return lines;
    });
    let lines = handle.await?;
    let expected = vec!["Line 0", "Line 1", "Line 2", "Line 0", "Line 1"];
    assert_eq!(lines, expected);
    Ok(())
}

#[tokio::test]
async fn delete_while_read() -> Result<()> {
    use tokio::time::timeout;
    let filename = "delete_while_read";
    // touch(file).await?;
    write_test_file(filename, false, 5).await?;
    let file = WatchedFile::new(filename).await?;
    let handle = tokio::spawn(async move {
        let mut file = BufReader::new(file).lines();
        let mut lines = Vec::new();
        while let Some(line) = file.next_line().await.unwrap() {
            lines.push(line);
            if lines.len() == 3 {
                tokio::fs::remove_file(filename).await.unwrap();
            }
        }
        return lines;
    });
    let lines = timeout(std::time::Duration::from_secs(5), handle).await??;
    let expected = vec!["Line 0", "Line 1", "Line 2", "Line 3", "Line 4"];
    assert_eq!(lines, expected);
    Ok(())
}
