//! Optional bounded-time measurement for the local example host. Buffered CSV is
//! streamed outside app.update and the peer step. A normal host opens no file.
use std::{
    fs::File,
    io::{BufWriter, Write},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(super) struct TickLog {
    file: BufWriter<File>,
    previous: Option<Instant>,
    flush_at: Instant,
    origin: Instant,
    utc_us: u128,
}
impl TickLog {
    pub(super) fn open() -> std::io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("IS_WORKER_TICKS") else {
            return Ok(None);
        };
        let mut file = BufWriter::new(File::create(path)?);
        writeln!(
            file,
            "unix_us,active_us,start_interval_us,ecs_us,peer_us,pending_inputs"
        )?;
        let origin = Instant::now();
        let utc_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        Ok(Some(Self {
            file,
            previous: None,
            flush_at: origin,
            origin,
            utc_us,
        }))
    }
    pub(super) fn record(
        &mut self,
        start: Instant,
        elapsed: Duration,
        ecs: Duration,
        peers: Duration,
        pending: usize,
    ) -> std::io::Result<()> {
        let interval = self
            .previous
            .replace(start)
            .map_or(0, |last| (start - last).as_micros());
        writeln!(
            self.file,
            "{},{},{},{},{},{}",
            self.utc_us + (start - self.origin).as_micros(),
            elapsed.as_micros(),
            interval,
            ecs.as_micros(),
            peers.as_micros(),
            pending
        )?;
        if start >= self.flush_at {
            self.file.flush()?;
            self.flush_at = start + Duration::from_millis(250);
        }
        Ok(())
    }
}
