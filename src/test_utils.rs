use std::{path::Path, time::Duration};

use anyhow::bail;
use futures::Future;
use tokio::{select, time::{interval, sleep}};

pub async fn timeout_1s<F, T>(fut: F) -> anyhow::Result<T>
where
    F: Future<Output = T>,
{
    select!(
        _ = sleep(Duration::from_secs(1)) => bail!("timeout after 1s"),
        output = fut => Ok(output)
    )
}

// Blocks until file exists, the dumb way.
pub async fn path_exists<P>(path: P)
where
    P: AsRef<Path>,
{
    let mut interval = interval(Duration::from_millis(10));
    while !path.as_ref().try_exists().unwrap() {
        interval.tick().await;
    }
}
