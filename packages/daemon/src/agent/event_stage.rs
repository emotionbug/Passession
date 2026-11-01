use anyhow::{Context, Result};
use serde_json::Value;
use std::{fs::File, io::{BufReader, BufWriter, Seek, Write}};

const MEMORY_EVENTS: usize = 4096;
const MEMORY_BYTES: usize = 16 * 1024 * 1024;
type Reader = serde_json::StreamDeserializer<'static, serde_json::de::IoRead<BufReader<File>>, Value>;

/// Admission can await an RPC whose reply follows provider output on the same pipe.
/// Spilling preserves FIFO without either unbounded RAM or blocking that reply.
#[derive(Default)]
pub(super) struct EventStage {
    events: Vec<Value>,
    bytes: usize,
    spill: Option<BufWriter<File>>,
}

impl EventStage {
    pub(super) async fn push(&mut self, event: Value) -> Result<()> {
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
        let mut events = std::mem::take(&mut self.events);
        let spill = self.spill.take();
        let (writer, events) = tokio::task::spawn_blocking(move || {
            let mut writer = match spill {
                Some(writer) => writer,
                None => BufWriter::new(tempfile::tempfile().context("Creating pending event spool")?),
            };
            for event in events.drain(..) {
                serde_json::to_writer(&mut writer, &event).context("Writing pending event spool")?;
                writer.write_all(b"\n").context("Writing pending event separator")?;
            }
            Ok::<_, anyhow::Error>((writer, events))
        }).await??;
        self.spill = Some(writer);
        self.events = events;
        self.bytes = 0;
        Ok(())
    }

    pub(super) async fn replay(self) -> Result<Replay> {
        let disk = match self.spill {
            None => None,
            Some(mut writer) => Some(tokio::task::spawn_blocking(move || {
                writer.flush().context("Flushing pending event spool")?;
                let mut file = writer.into_inner().map_err(|error| error.into_error())?;
                file.rewind().context("Rewinding pending event spool")?;
                Ok::<_, anyhow::Error>(serde_json::Deserializer::from_reader(BufReader::new(file)).into_iter())
            }).await??),
        };
        Ok(Replay { disk, tail: self.events })
    }
}

pub(super) struct Replay {
    disk: Option<Reader>,
    tail: Vec<Value>,
}

impl Replay {
    pub(super) async fn next_batch(&mut self) -> Result<Vec<Value>> {
        if let Some(mut reader) = self.disk.take() {
            let (reader, batch) = tokio::task::spawn_blocking(move || {
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
            }).await??;
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
}
