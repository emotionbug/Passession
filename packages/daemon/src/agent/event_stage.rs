use anyhow::{Context, Result};
use serde_json::Value;
use std::{fs::File, io::{BufReader, BufWriter, Seek, Write}};
use tokio::task::JoinHandle;

const MEMORY_EVENTS: usize = 4096;
const MEMORY_BYTES: usize = 16 * 1024 * 1024;
type Reader = serde_json::StreamDeserializer<'static, serde_json::de::IoRead<BufReader<File>>, Value>;
type Spilled = (BufWriter<File>, Vec<Value>);
type ReadBatch = (Option<Reader>, Vec<Value>);

/// Admission can await an RPC whose reply follows provider output on the same pipe.
/// Spilling preserves FIFO without either unbounded RAM or blocking that reply.
///
/// Blocking file work owns moved rows until it finishes. Its handle stays in the
/// stage, so a caller that drops an await (for example a losing `select!` arm)
/// leaves those accepted rows resumable instead of silently discarding them.
#[derive(Default)]
pub(crate) struct EventStage {
    events: Vec<Value>,
    bytes: usize,
    spill: Option<BufWriter<File>>,
    flushing: Option<JoinHandle<Result<Spilled>>>,
}

impl EventStage {
    pub(crate) async fn push(&mut self, event: Value) -> Result<()> {
        self.settle().await?;
        let bytes = event_size(&event);
        if !self.events.is_empty()
            && (self.events.len() >= MEMORY_EVENTS || self.bytes.saturating_add(bytes) > MEMORY_BYTES)
        {
            self.flush().await?;
        }
        self.events.push(event);
        self.bytes += bytes;
        // A single provider frame can exceed the memory threshold by itself.
        if self.bytes > MEMORY_BYTES {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.settle().await?;
        let mut events = std::mem::take(&mut self.events);
        let spill = self.spill.take();
        self.bytes = 0;
        self.flushing = Some(tokio::task::spawn_blocking(move || {
            let mut writer = match spill {
                Some(writer) => writer,
                None => BufWriter::new(tempfile::tempfile().context("Creating pending event spool")?),
            };
            for event in events.drain(..) {
                serde_json::to_writer(&mut writer, &event).context("Writing pending event spool")?;
                writer.write_all(b"\n").context("Writing pending event separator")?;
            }
            Ok::<_, anyhow::Error>((writer, events))
        }));
        self.settle().await
    }

    /// Completes a flush whose original awaiting caller may have been dropped.
    /// Rows pushed after it stay in the memory tail, behind the spilled rows.
    async fn settle(&mut self) -> Result<()> {
        let Some(task) = self.flushing.as_mut() else { return Ok(()) };
        let result = task.await;
        self.flushing = None;
        let (writer, events) = result.context("Pending event spool writer stopped")??;
        self.spill = Some(writer);
        if self.events.is_empty() {
            // Reuse the drained allocation.
            self.events = events;
        }
        Ok(())
    }

    pub(crate) async fn replay(mut self) -> Result<Replay> {
        self.settle().await?;
        let disk = match self.spill {
            None => None,
            Some(mut writer) => Some(tokio::task::spawn_blocking(move || {
                writer.flush().context("Flushing pending event spool")?;
                let mut file = writer.into_inner().map_err(|error| error.into_error())?;
                file.rewind().context("Rewinding pending event spool")?;
                Ok::<_, anyhow::Error>(serde_json::Deserializer::from_reader(BufReader::new(file)).into_iter())
            }).await??),
        };
        Ok(Replay { disk, reading: None, tail: self.events })
    }
}

pub(crate) struct Replay {
    disk: Option<Reader>,
    /// The in-flight disk read, retained across a dropped `next_batch` await.
    reading: Option<JoinHandle<Result<ReadBatch>>>,
    tail: Vec<Value>,
}

impl Replay {
    pub(crate) async fn next_batch(&mut self) -> Result<Vec<Value>> {
        if self.reading.is_none() {
            if let Some(mut reader) = self.disk.take() {
                self.reading = Some(tokio::task::spawn_blocking(move || {
                    let mut batch = Vec::new();
                    let mut bytes = 0usize;
                    while batch.len() < MEMORY_EVENTS && bytes < MEMORY_BYTES {
                        let Some(event) = reader.next() else {
                            return Ok::<_, anyhow::Error>((None, batch));
                        };
                        let event = event.context("Reading pending event spool")?;
                        bytes = bytes.saturating_add(event_size(&event));
                        batch.push(event);
                    }
                    Ok((Some(reader), batch))
                }));
            }
        }
        if let Some(task) = self.reading.as_mut() {
            let result = task.await;
            self.reading = None;
            let (reader, batch) = result.context("Pending event spool reader stopped")??;
            self.disk = reader;
            if !batch.is_empty() {
                return Ok(batch);
            }
        }
        Ok(std::mem::take(&mut self.tail))
    }
}

fn event_size(value: &Value) -> usize {
    match value {
        Value::String(value) => value.len(),
        Value::Array(values) => values.iter().map(event_size).sum(),
        Value::Object(values) => values.iter().map(|(key, value)| key.len() + event_size(value)).sum(),
        _ => 16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn spills_by_count_and_replays_disk_before_memory_tail() {
        let mut stage = EventStage::default();
        for index in 0..MEMORY_EVENTS * 2 + 3 {
            stage.push(json!({"index":index,"text":format!("row {index}")})).await.unwrap();
        }
        assert!(stage.spill.is_some());
        assert!(stage.events.len() <= MEMORY_EVENTS);
        let mut replay = stage.replay().await.unwrap();
        let mut index = 0;
        loop {
            let batch = replay.next_batch().await.unwrap();
            if batch.is_empty() { break; }
            for event in batch {
                assert_eq!(event, json!({"index":index,"text":format!("row {index}")}));
                index += 1;
            }
        }
        assert_eq!(index, MEMORY_EVENTS * 2 + 3);
    }

    #[tokio::test]
    async fn spills_large_frames_without_truncation_or_reordering() {
        let mut stage = EventStage::default();
        let sizes = [MEMORY_BYTES / 2, MEMORY_BYTES / 2, MEMORY_BYTES + 1, 7];
        for (index, size) in sizes.into_iter().enumerate() {
            stage.push(json!({"index":index,"text":"x".repeat(size)})).await.unwrap();
            assert!(stage.bytes <= MEMORY_BYTES);
        }
        let mut replay = stage.replay().await.unwrap();
        let mut index = 0;
        loop {
            let batch = replay.next_batch().await.unwrap();
            if batch.is_empty() { break; }
            for event in batch {
                assert_eq!(event["index"], index);
                let text = event["text"].as_str().unwrap();
                assert_eq!(text.len(), sizes[index]);
                assert!(text.bytes().all(|byte| byte == b'x'));
                index += 1;
            }
        }
        assert_eq!(index, sizes.len());
    }

    #[tokio::test]
    async fn abandoned_spill_and_replay_awaits_keep_accepted_rows_in_order() {
        use std::task::Poll;
        let row = |index: usize| json!({"index":index,"text":format!("row {index}")});
        let mut stage = EventStage::default();
        let mut expected: Vec<usize> = (0..MEMORY_EVENTS).collect();
        for index in 0..MEMORY_EVENTS {
            stage.push(row(index)).await.unwrap();
        }
        // This push first spills the full memory tail. A losing select arm drops
        // it while the blocking writer owns those already accepted rows.
        let abandoned = MEMORY_EVENTS;
        {
            let mut push = std::pin::pin!(stage.push(row(abandoned)));
            if let Poll::Ready(result) = futures_util::poll!(push.as_mut()) {
                result.unwrap();
                expected.push(abandoned);
            }
        }
        for index in abandoned + 1..abandoned + 4 {
            stage.push(row(index)).await.unwrap();
            expected.push(index);
        }
        let mut replay = stage.replay().await.unwrap();
        let mut seen = Vec::new();
        {
            let mut read = std::pin::pin!(replay.next_batch());
            if let Poll::Ready(batch) = futures_util::poll!(read.as_mut()) {
                seen.extend(batch.unwrap());
            }
        }
        loop {
            let batch = replay.next_batch().await.unwrap();
            if batch.is_empty() { break; }
            seen.extend(batch);
        }
        for (event, index) in seen.iter().zip(&expected) {
            assert_eq!(event, &row(*index));
        }
        assert_eq!(seen.len(), expected.len());
    }
}
