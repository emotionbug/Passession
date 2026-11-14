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
/// Spilling preserves FIFO while budgeting retained events without blocking that reply.
/// The byte budget estimates event storage, not allocator/RSS usage: opaque map
/// capacity, allocator overhead, spool buffers and the stage Vec's spare slots
/// are outside this estimate. A single incoming/deserialized frame can exceed the
/// budget; replay stops after the frame that crosses it.
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
        self.bytes = self.bytes.saturating_add(bytes);
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
        if self.reading.is_none()
            && let Some(mut reader) = self.disk.take() {
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
    // Inline Values are charged once: here for the root, by Vec capacity for
    // array elements, and by the entry estimate for object members.
    std::mem::size_of::<Value>().saturating_add(event_heap_size(value))
}

fn event_heap_size(value: &Value) -> usize {
    match value {
        Value::String(value) => value.capacity(),
        Value::Array(values) => values.iter().fold(
            values.capacity().saturating_mul(std::mem::size_of::<Value>()),
            |bytes, value| bytes.saturating_add(event_heap_size(value)),
        ),
        Value::Object(values) => {
            // serde_json hides BTreeMap/IndexMap capacity. Budget two entry
            // slots and four words per live entry for slack, links and indices,
            // plus sixteen entry slots for a nonempty map's initial allocation.
            // This conservative estimate works with either backend, but cannot
            // bound arbitrary capacity retained after removals or allocator costs.
            let entry = std::mem::size_of::<(String, Value)>();
            let storage = if values.is_empty() {
                0
            } else {
                entry.saturating_mul(16).saturating_add(
                    values.len().saturating_mul(
                        entry.saturating_mul(2).saturating_add(
                            std::mem::size_of::<usize>().saturating_mul(4),
                        ),
                    ),
                )
            };
            values.iter().fold(storage, |bytes, (key, value)| {
                bytes.saturating_add(key.capacity()).saturating_add(event_heap_size(value))
            })
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn structural_row(index: usize) -> Value {
        let slots = MEMORY_BYTES / 3 / std::mem::size_of::<Value>() + 1;
        let mut values = Vec::with_capacity(slots);
        values.push(Value::from(index));
        values.resize_with(slots, || Value::Object(Default::default()));
        Value::Array(values)
    }

    #[tokio::test]
    async fn container_heavy_frames_spill_and_replay_in_bounded_fifo_batches() {
        let mut stage = EventStage::default();
        for index in 0..10 {
            stage.push(structural_row(index)).await.unwrap();
        }
        assert!(stage.spill.is_some());
        assert!(stage.events.len() < 10);
        stage.push(json!("memory tail")).await.unwrap();
        let mut replay = stage.replay().await.unwrap();
        let mut index = 0;
        loop {
            let batch = replay.next_batch().await.unwrap();
            if batch.is_empty() { break; }
            // Independently count only the array slots, excluding map estimates.
            // Replay may cross its budget by one complete frame, never by more.
            let mut slots_before_last = 0usize;
            for event in batch.iter().take(batch.len() - 1) {
                if let Some(values) = event.as_array() {
                    slots_before_last += values.capacity() * std::mem::size_of::<Value>();
                }
            }
            assert!(slots_before_last < MEMORY_BYTES);
            for event in batch {
                if index < 10 {
                    assert_eq!(event, structural_row(index));
                } else {
                    assert_eq!(index, 10);
                    assert_eq!(event, json!("memory tail"));
                }
                index += 1;
            }
        }
        assert_eq!(index, 11);
    }

    #[tokio::test]
    async fn reserved_capacity_spills_even_when_payloads_are_small() {
        for kind in 0..3 {
            let event = match kind {
                0 => {
                    let mut text = String::with_capacity(MEMORY_BYTES + 1);
                    text.push_str("payload");
                    Value::String(text)
                }
                1 => {
                    let slots = MEMORY_BYTES / std::mem::size_of::<Value>() + 1;
                    Value::Array(vec![Value::Array(Vec::with_capacity(slots))])
                }
                _ => {
                    let mut key = String::with_capacity(MEMORY_BYTES + 1);
                    key.push_str("key");
                    let mut values = serde_json::Map::new();
                    values.insert(key, Value::Null);
                    Value::Object(values)
                }
            };
            let expected = event.clone();
            let mut stage = EventStage::default();
            stage.push(event).await.unwrap();
            assert!(stage.spill.is_some());
            assert!(stage.events.is_empty());
            let mut replay = stage.replay().await.unwrap();
            assert_eq!(replay.next_batch().await.unwrap(), vec![expected]);
            assert!(replay.next_batch().await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn array_slots_fit_once_then_spill_at_the_memory_boundary() {
        let slots = MEMORY_BYTES / std::mem::size_of::<Value>() - 1;
        let mut values = Vec::with_capacity(slots);
        values.resize(slots, Value::Null);
        let mut stage = EventStage::default();
        stage.push(Value::Array(values)).await.unwrap();
        // Counting each child inline Value again would spill this fitting frame.
        assert!(stage.spill.is_none());
        stage.push(Value::Bool(true)).await.unwrap();
        assert!(stage.spill.is_some());
        let mut replay = stage.replay().await.unwrap();
        let batch = replay.next_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        let values = batch[0].as_array().unwrap();
        assert_eq!(values.len(), slots);
        assert!(values.iter().all(Value::is_null));
        assert_eq!(replay.next_batch().await.unwrap(), vec![Value::Bool(true)]);
        assert!(replay.next_batch().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn object_entry_storage_spills_without_large_strings() {
        let entries = MEMORY_BYTES / std::mem::size_of::<(String, Value)>() + 1;
        let values = (0..entries).map(|index| (index.to_string(), Value::Null)).collect();
        let mut stage = EventStage::default();
        stage.push(Value::Object(values)).await.unwrap();
        assert!(stage.spill.is_some());
        let mut replay = stage.replay().await.unwrap();
        let batch = replay.next_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        let values = batch[0].as_object().unwrap();
        assert_eq!(values.len(), entries);
        assert!(values.iter().all(|(key, value)| {
            key.parse::<usize>().unwrap() < entries && value.is_null()
        }));
        assert!(replay.next_batch().await.unwrap().is_empty());
    }

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
        for container_heavy in [false, true] {
            let row = |index: usize| {
                if container_heavy {
                    structural_row(index)
                } else {
                    json!({"index":index,"text":format!("row {index}")})
                }
            };
            let limit = if container_heavy { 2 } else { MEMORY_EVENTS };
            let mut stage = EventStage::default();
            let mut expected: Vec<usize> = (0..limit).collect();
            for index in 0..limit {
                stage.push(row(index)).await.unwrap();
            }
            assert!(stage.spill.is_none());
            // Drop the next push while its writer owns the accepted memory tail.
            let abandoned = limit;
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
}
