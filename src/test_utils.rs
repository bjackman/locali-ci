use std::{path::Path, time::Duration};

use anyhow::bail;
use futures::Future;
use tokio::{
    select,
    time::{interval, sleep},
};

pub async fn timeout_5s<F, T>(fut: F) -> anyhow::Result<T>
where
    F: Future<Output = T>,
{
    select! {
        _ = sleep(Duration::from_secs(5)) => bail!("timeout after 5s"),
        output = fut => Ok(output)
    }
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

// Sick of shitty test harness and shitty logging framework, just use this macro
// to log to stderr with a timestamp. And because of the shitty inability to
// share code between integration test and unit tests, this is duplicated.
#[allow(unused_macros)]
macro_rules! eprintln_ts {
    ($($arg:tt)*) => {
        {
            eprintln!("[{}] {}", Local::now().to_rfc3339(), format!($($arg)*));
        }
    }
}
