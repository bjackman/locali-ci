use std::{future::pending, io::stdout, sync::RwLock};

use anyhow::Context as _;
use async_stream::try_stream;
use crossterm::{
    event::{Event, EventStream},
    terminal,
    tty::IsTty as _,
};
use futures::{Stream, StreamExt as _};

use crate::util::Rect;

pub struct TerminalSizeWatcher {
    size: RwLock<Rect>,
}

impl<'a> TerminalSizeWatcher {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            size: RwLock::new(if !stdout().is_tty() {
                Rect { cols: 0, rows: 0 }
            } else {
                // TODO: We're importing crossterm just for this lol
                let (cols, rows) = terminal::size().context("getting terminal size")?;
                Rect {
                    cols: cols.into(),
                    rows: rows.into(),
                }
            }),
        })
    }

    // Returns an item whenever the terminal gets resized. When that happens you
    // should call size again
    pub fn resizes(&'a self) -> impl Stream<Item = anyhow::Result<()>> + use<'a> {
        try_stream! {
            // crossterm async code seems to be buggy when not a tty.
            if !stdout().is_tty() {
                pending::<()>().await; // Block forever.
                yield ();
            };
            let mut reader = EventStream::new();
            loop {
                let event: Event = reader.next().await
                    .context("terminal event stream terminated")?
                    .context("error reading terminal events")?;
                if let Event::Resize(cols, rows) = event {
                    *self.size.write().unwrap() = Rect {cols: cols.into(), rows: rows.into()};
                    yield ();
                }
            }
        }
    }

    // Get the current size of the terminal
    pub fn size(&self) -> Rect {
        self.size.read().unwrap().clone()
    }
}
