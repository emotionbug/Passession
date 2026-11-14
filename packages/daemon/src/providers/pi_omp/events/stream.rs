//! OMP message updates use RPC message IDs, not the response ID learned at message_end.
use super::{array, first, string};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

#[derive(Default)]
pub(super) struct Streams {
    messages: HashMap<String, Message>,
    // Older OMP sends no RPC message ID and has only one foreground message.
    anonymous: Option<Message>,
}

struct Message {
    id: String,
    blocks: BTreeMap<usize, Block>,
}

#[derive(Default)]
struct Block {
    text: String,
    thinking: bool,
    emitted: usize,
    closed: bool,
    authoritative: bool,
    mismatch_reported: bool,
}

impl Message {
    fn new(message: &Value) -> Self {
        Self {
            id: first(&[&message["responseId"]])
                .map(str::to_owned)
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            blocks: BTreeMap::new(),
        }
    }

    fn append(&mut self, index: usize, thinking: bool, text: &str) -> Vec<Value> {
        if text.is_empty() {
            return Vec::new();
        }
        let later_emitted = self.later_emitted(index);
        let block = self.blocks.entry(index).or_default();
        if later_emitted || (!block.text.is_empty() && block.thinking != thinking) {
            return block.mismatch(&self.id, index).into_iter().collect();
        }
        block.thinking = thinking;
        block.text.push_str(text);
        block.authoritative = false;
        self.drain()
    }

    fn later_emitted(&self, index: usize) -> bool {
        use std::ops::Bound::{Excluded, Unbounded};
        self.blocks
            .range((Excluded(index), Unbounded))
            .any(|(_, block)| block.emitted != 0)
    }

    fn drain(&mut self) -> Vec<Value> {
        let mut items = Vec::new();
        for block in self.blocks.values_mut() {
            // OMP streams summary separators that its final thinking body trims.
            // Keep that tail unpublished until it becomes internal whitespace or
            // an authoritative body confirms it, rather than altering its bytes.
            let end = if block.thinking && !block.authoritative {
                block.text.trim_end().len()
            } else {
                block.text.len()
            };
            if block.emitted < end {
                items.push(text_item(
                    &self.id,
                    block.thinking,
                    &block.text[block.emitted..end],
                ));
                block.emitted = end;
            }
            // A later content block cannot overtake an unfinished earlier block.
            if !block.closed || block.emitted < block.text.len() {
                break;
            }
        }
        items
    }

    fn reconcile(
        &mut self,
        index: usize,
        thinking: bool,
        text: &str,
        authoritative: bool,
    ) -> Option<Value> {
        let later_emitted = self.later_emitted(index);
        let block = self.blocks.entry(index).or_default();
        if authoritative {
            if !text.starts_with(&block.text[..block.emitted])
                || (block.emitted != 0 && block.thinking != thinking)
                || (later_emitted && text.len() != block.emitted)
            {
                block.text.truncate(block.emitted);
                return block.mismatch(&self.id, index);
            }
            // Only published bytes constrain reconciliation. Buffered later blocks
            // can still be replaced by their authoritative content without loss.
            block.thinking = thinking;
            block.text.truncate(block.emitted);
            block.text.push_str(&text[block.emitted..]);
            block.authoritative = true;
        } else if let Some(suffix) = text.strip_prefix(block.text.as_str())
            && !later_emitted
            && (block.text.is_empty() || block.thinking == thinking)
        {
            block.thinking = thinking;
            block.text.push_str(suffix);
            if !suffix.is_empty() {
                block.authoritative = false;
            }
        }
        None
    }

    fn snapshot(&mut self, message: &Value, authoritative: bool) -> Vec<Value> {
        let mut items = Vec::new();
        for (index, block) in array(&message["content"]).iter().enumerate() {
            let (thinking, text) = match string(&block["type"]) {
                "text" => (false, string(&block["text"])),
                "thinking" => (true, string(&block["thinking"])),
                _ => continue,
            };
            items.extend(self.reconcile(index, thinking, text, authoritative));
            self.blocks.get_mut(&index).unwrap().closed =
                authoritative || index + 1 < array(&message["content"]).len();
        }
        if authoritative {
            let content = array(&message["content"]);
            for (&index, block) in &mut self.blocks {
                if !block.text.is_empty()
                    && content.get(index).is_none_or(|value| {
                        value["type"] != if block.thinking { "thinking" } else { "text" }
                    })
                {
                    if block.emitted != 0 {
                        items.extend(block.mismatch(&self.id, index));
                    }
                    block.text.truncate(block.emitted);
                    block.closed = true;
                }
            }
        }
        items.extend(self.drain());
        items
    }
}

fn text_item(id: &str, thinking: bool, text: &str) -> Value {
    let mut item = json!({
        "type": if thinking { "reasoning" } else { "assistant_message" },
        "text": text,
    });
    if !thinking {
        item["messageId"] = json!(id);
    }
    item
}

impl Block {
    fn mismatch(&mut self, message: &str, index: usize) -> Option<Value> {
        if self.mismatch_reported {
            return None;
        }
        self.mismatch_reported = true;
        // The neutral stream is append-only. Never duplicate a conflicting final body.
        Some(json!({
            "type": "error",
            "message": format!("OMP authoritative content differs from streamed message {message}, block {index}; reload native history to recover the final content."),
        }))
    }
}

impl Streams {
    pub(super) fn clear(&mut self) {
        self.messages.clear();
        self.anonymous = None;
    }

    fn message(&mut self, frame: &Value) -> &mut Message {
        if let Some(id) = frame["messageId"].as_str() {
            if !self.messages.contains_key(id) {
                self.messages
                    .insert(id.into(), Message::new(&frame["message"]));
            }
            self.messages.get_mut(id).unwrap()
        } else {
            self.anonymous
                .get_or_insert_with(|| Message::new(&frame["message"]))
        }
    }

    pub(super) fn start(&mut self, frame: &Value) -> Vec<Value> {
        if frame["messageId"].as_str().is_none() {
            self.anonymous = None;
        }
        self.message(frame).snapshot(&frame["message"], false)
    }

    pub(super) fn update(&mut self, frame: &Value) -> Vec<Value> {
        let message = self.message(frame);
        if frame["message"]["content"].is_array() {
            // Full-mode snapshots can include subsequent deltas already queued by OMP.
            return message.snapshot(&frame["message"], false);
        }
        let update = &frame["assistantMessageEvent"];
        let kind = string(&update["type"]);
        let thinking = match kind {
            "text_start" | "text_delta" | "text_end" => false,
            "thinking_start" | "thinking_delta" | "thinking_end" => true,
            _ => return Vec::new(),
        };
        let index = update["contentIndex"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        match kind {
            "text_delta" | "thinking_delta" => {
                message.append(index, thinking, string(&update["delta"]))
            }
            _ => {
                let complete = matches!(kind, "text_end" | "thinking_end");
                let text = if complete {
                    update["content"].as_str()
                } else {
                    update["block"][if thinking { "thinking" } else { "text" }].as_str()
                };
                // Delta-mode end events may omit content. Only a supplied body,
                // including an explicit empty string, is authoritative.
                let mut items: Vec<_> = message
                    .reconcile(
                        index,
                        thinking,
                        text.unwrap_or_default(),
                        complete && text.is_some(),
                    )
                    .into_iter()
                    .collect();
                if complete {
                    message.blocks.get_mut(&index).unwrap().closed = true;
                }
                items.extend(message.drain());
                items
            }
        }
    }

    pub(super) fn finish(&mut self, frame: &Value) -> Vec<Value> {
        let items = self.message(frame).snapshot(&frame["message"], true);
        if let Some(id) = frame["messageId"].as_str() {
            self.messages.remove(id);
        } else {
            self.anonymous = None;
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::super::Mapper;
    use serde_json::{Value, json};

    fn start(id: &str, content: Value) -> Value {
        json!({"type":"message_start","messageId":id,"message":{"role":"assistant","content":content}})
    }

    fn update(id: &str, index: usize, kind: &str, value: &str) -> Value {
        json!({"type":"message_update","messageId":id,"message":{"role":"assistant"},
            "assistantMessageEvent":{"type":kind,"contentIndex":index,"delta":value,"content":value}})
    }

    fn end(id: &str, content: Value) -> Value {
        json!({"type":"message_end","messageId":id,
            "message":{"role":"assistant","responseId":"late-response","content":content,"stopReason":"stop"}})
    }

    fn rendered(events: &[Value], kind: &str) -> String {
        events
            .iter()
            .filter(|event| event["item"]["type"] == kind)
            .filter_map(|event| event["item"]["text"].as_str())
            .collect()
    }

    #[test]
    fn omp_delta_unicode_prefix_shells_and_terminal_suffix_are_emitted_once() {
        let mut mapper = Mapper::new("omp", "custom-omp");
        mapper.begin_turn("turn");
        let mut events = mapper.map(&start("msg-2", json!([{"type":"text","text":"원본 "}])));
        let id = events[0]["item"]["messageId"].clone();
        let shell = json!({"type":"message_update","messageId":"msg-2","message":{"role":"assistant"},
            "assistantMessageEvent":{"type":"text_start","contentIndex":0,
                "block":{"type":"text","text":"원본 ","textSignature":"native-signature"}}});
        assert!(mapper.map(&shell).is_empty());
        for delta in ["대화", "🙂", "e", "\u{301}", "하", "하"] {
            events.extend(mapper.map(&update("msg-2", 0, "text_delta", delta)));
        }
        events.extend(mapper.map(&end(
            "msg-2",
            json!([{"type":"text","text":"원본 대화🙂e\u{301}하하 끝"}]),
        )));
        assert_eq!(
            rendered(&events, "assistant_message"),
            "원본 대화🙂e\u{301}하하 끝"
        );
        assert!(events.iter().all(|event| event["provider"] == "custom-omp"
            && event["turnId"] == "turn"
            && event["item"]["messageId"] == id));
        assert_eq!(
            mapper
                .map(&json!({"type":"agent_end","messages":[]}))
                .last()
                .unwrap()["type"],
            "turn_completed"
        );
    }

    #[test]
    fn omp_delta_multiple_text_thinking_blocks_and_tool_shells_preserve_order() {
        let mut mapper = Mapper::new("omp", "omp");
        mapper.begin_turn("turn");
        mapper.map(&start("msg-2", json!([])));
        let mut events = Vec::new();
        for (index, kind, prefix, suffix) in [
            (0, "thinking", "생각", " 하나"),
            (1, "text", "한글", " 답변"),
            (3, "thinking", "추가", " 생각"),
            (4, "text", "다음", " 답변"),
        ] {
            let shell = json!({"type":"message_update","messageId":"msg-2","message":{"role":"assistant"},
                "assistantMessageEvent":{"type":format!("{kind}_start"),"contentIndex":index,
                    "block":{"type":kind}}});
            assert!(mapper.map(&shell).is_empty());
            events.extend(mapper.map(&update("msg-2", index, &format!("{kind}_delta"), prefix)));
            events.extend(mapper.map(&update(
                "msg-2",
                index,
                &format!("{kind}_end"),
                &format!("{prefix}{suffix}"),
            )));
            if index == 1 {
                for kind in ["toolcall_start", "toolcall_delta", "toolcall_end"] {
                    assert!(mapper.map(&json!({"type":"message_update","messageId":"msg-2","message":{"role":"assistant"},
                        "assistantMessageEvent":{"type":kind,"contentIndex":2,"delta":"{\"query\":\"한글\"}",
                            "block":{"type":"toolCall","id":"call","name":"host_tool"}}})).is_empty());
                }
            }
        }
        let content = json!([
            {"type":"thinking","thinking":"생각 하나"}, {"type":"text","text":"한글 답변"},
            {"type":"toolCall","id":"call","name":"host_tool","arguments":{"query":"한글"}},
            {"type":"thinking","thinking":"추가 생각"}, {"type":"text","text":"다음 답변"}
        ]);
        assert!(mapper.map(&end("msg-2", content)).is_empty());
        assert_eq!(rendered(&events, "assistant_message"), "한글 답변다음 답변");
        assert_eq!(rendered(&events, "reasoning"), "생각 하나추가 생각");
        let chunks: Vec<_> = events
            .iter()
            .map(|event| event["item"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            chunks,
            [
                "생각", " 하나", "한글", " 답변", "추가", " 생각", "다음", " 답변"
            ]
        );
        let running = mapper.map(&json!({"type":"tool_execution_start","toolCallId":"call","toolName":"host_tool","args":{"query":"한글"}}));
        assert_eq!(
            running[0]["item"]["detail"]["input"],
            json!({"query":"한글"})
        );
        let done = mapper.map(
            &json!({"type":"tool_execution_end","toolCallId":"call","toolName":"host_tool",
            "result":{"content":[{"type":"text","text":"검색 결과"}]}}),
        );
        assert_eq!(done[0]["item"]["status"], "completed");
        assert_eq!(done[0]["item"]["detail"]["input"], json!({"query":"한글"}));
        assert_eq!(
            done[0]["item"]["detail"]["output"]["content"][0]["text"],
            "검색 결과"
        );
    }

    #[test]
    fn omp_delta_interleaved_message_and_block_state_never_blends() {
        let mut mapper = Mapper::new("omp", "omp");
        mapper.begin_turn("turn");
        mapper.map(&start("a", json!([])));
        let a = mapper.map(&update("a", 0, "text_delta", "첫"));
        mapper.map(&start("b", json!([])));
        let b = mapper.map(&update("b", 0, "text_delta", "둘"));
        assert_ne!(a[0]["item"]["messageId"], b[0]["item"]["messageId"]);
        let a_end = mapper.map(&end("a", json!([{"type":"text","text":"첫째"}])));
        let b_more = mapper.map(&update("b", 0, "text_delta", "째"));
        assert_eq!(a_end[0]["item"]["messageId"], a[0]["item"]["messageId"]);
        assert_eq!(a_end[0]["item"]["text"], "째");
        assert_eq!(b_more[0]["item"]["messageId"], b[0]["item"]["messageId"]);
        assert!(
            mapper
                .map(&end("b", json!([{"type":"text","text":"둘째"}])))
                .is_empty()
        );

        mapper.map(&start("c", json!([])));
        mapper.map(&start("d", json!([])));
        mapper.map(&update("c", 0, "thinking_delta", "생각"));
        mapper.map(&update("d", 0, "thinking_delta", "계획"));
        assert_eq!(
            rendered(
                &mapper.map(&end("c", json!([{"type":"thinking","thinking":"생각 끝"}]))),
                "reasoning"
            ),
            " 끝"
        );
        assert_eq!(
            rendered(
                &mapper.map(&end(
                    "d",
                    json!([{"type":"thinking","thinking":"계획 완료"}])
                )),
                "reasoning"
            ),
            " 완료"
        );
    }

    #[test]
    fn omp_delta_terminal_only_and_unstreamed_trailing_blocks_match_replay_content() {
        let mut mapper = Mapper::new("omp", "omp");
        let message = end(
            "msg-2",
            json!([
                {"type":"thinking","thinking":"생각🙂"}, {"type":"text","text":"한글 답변"},
                {"type":"thinking","thinking":"다른 생각"}, {"type":"text","text":"끝"}
            ]),
        );
        let events = mapper.map(&message);
        let replay = Mapper::new("omp", "omp").replay_message(&message["message"], None);
        assert_eq!(events, replay);

        mapper.map(&start("msg-3", json!([])));
        let mut streamed = mapper.map(&update("msg-3", 0, "text_delta", "먼저"));
        streamed.extend(mapper.map(&end(
            "msg-3",
            json!([
                {"type":"text","text":"먼저 완료"}, {"type":"thinking","thinking":"나중 생각"},
                {"type":"text","text":"마지막"}
            ]),
        )));
        assert_eq!(rendered(&streamed, "assistant_message"), "먼저 완료마지막");
        assert_eq!(rendered(&streamed, "reasoning"), "나중 생각");
    }

    #[test]
    fn omp_delta_reset_resume_and_reused_rpc_ids_do_not_reuse_stream_prefixes() {
        let mut mapper = Mapper::new("omp", "omp");
        mapper.begin_turn("before");
        mapper.map(&start("msg-2", json!([])));
        let before = mapper.map(&update("msg-2", 0, "text_delta", "이전"));
        // Runtime restarts reuse msg-N but must not reset the turn lifecycle.
        mapper.reset_streams();
        assert_eq!(mapper.active_turn_id(), Some("before"));
        mapper.map(&start("msg-2", json!([])));
        let resumed = mapper.map(&update("msg-2", 0, "text_delta", "이전"));
        assert_eq!(rendered(&resumed, "assistant_message"), "이전");
        assert_ne!(
            before[0]["item"]["messageId"],
            resumed[0]["item"]["messageId"]
        );
        let history = end("history", json!([{"type":"text","text":"기록"}]));
        mapper.replay_message(&history["message"], None);
        assert_eq!(
            rendered(
                &mapper.map(&end("msg-2", json!([{"type":"text","text":"이전 재개"}]))),
                "assistant_message"
            ),
            " 재개"
        );
        mapper.end_turn();
        mapper.begin_turn("after");
        assert_eq!(
            rendered(
                &mapper.map(&end("msg-2", json!([{"type":"text","text":"이전"}]))),
                "assistant_message"
            ),
            "이전"
        );
        mapper.map(&update("abandoned", 0, "text_delta", "중단"));
        mapper.begin_turn("replacement");
        assert_eq!(
            rendered(
                &mapper.map(&end("abandoned", json!([{"type":"text","text":"중단"}]))),
                "assistant_message"
            ),
            "중단"
        );
    }

    #[test]
    fn omp_codex_summary_separator_is_not_published_before_authoritative_end() {
        // OMP 18.18.8, protocol 2 delta mode, openai-codex/gpt-6-astra high:
        // the summary and separator are separate deltas; neither end retains it.
        let summary = "**합동식을 대입해 최소해를 계산합니다**";
        let mut mapper = Mapper::new("omp", "omp");
        mapper.map(&start("msg-2", json!([{"type":"thinking","thinking":""}])));
        assert!(mapper.map(&json!({"type":"message_update","messageId":"msg-2",
            "message":{"role":"assistant"},
            "assistantMessageEvent":{"type":"thinking_start","contentIndex":0,
                "block":{"type":"thinking","thinking":""}}})).is_empty());
        let mut events = mapper.map(&json!({"type":"message_update","messageId":"msg-2",
            "message":{"role":"assistant"},
            "assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":summary}}));
        assert_eq!(rendered(&events, "reasoning"), summary);
        assert!(mapper.map(&json!({"type":"message_update","messageId":"msg-2",
            "message":{"role":"assistant"},
            "assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"\n\n"}})).is_empty());
        assert!(mapper.map(&json!({"type":"message_update","messageId":"msg-2",
            "message":{"role":"assistant"},
            "assistantMessageEvent":{"type":"thinking_end","contentIndex":0,"content":summary,
                "block":{"type":"thinking","summary":true}}})).is_empty());
        events.extend(mapper.map(&update("msg-2", 1, "text_delta", "가장 작은 양의 정수는 **423**입니다.")));
        assert!(mapper.map(&end("msg-2", json!([
            {"type":"thinking","thinking":summary,"summary":true},
            {"type":"text","text":"가장 작은 양의 정수는 **423**입니다."}
        ]))).is_empty());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["item"]["type"], "reasoning");
        assert_eq!(events[1]["item"]["type"], "assistant_message");
    }

    #[test]
    fn omp_multiple_thinking_blocks_keep_summary_boundaries_in_order() {
        let mut mapper = Mapper::new("omp", "omp");
        let mut events = Vec::new();
        for (index, summary) in ["첫 요약🙂", "두 번째\n\n요약", "마지막 요약"].iter().enumerate() {
            events.extend(mapper.map(&update("msg-2", index, "thinking_delta", summary)));
            assert!(mapper.map(&update("msg-2", index, "thinking_delta", "\n\n")).is_empty());
            assert!(mapper.map(&update("msg-2", index, "thinking_end", summary)).is_empty());
        }
        events.extend(mapper.map(&update("msg-2", 3, "text_delta", "답변")));
        assert!(mapper.map(&end("msg-2", json!([
            {"type":"thinking","thinking":"첫 요약🙂"},
            {"type":"thinking","thinking":"두 번째\n\n요약"},
            {"type":"thinking","thinking":"마지막 요약"},
            {"type":"text","text":"답변"}
        ]))).is_empty());
        let chunks: Vec<_> = events.iter().map(|event| (
            event["item"]["type"].as_str().unwrap(),
            event["item"]["text"].as_str().unwrap(),
        )).collect();
        assert_eq!(chunks, [
            ("reasoning", "첫 요약🙂"),
            ("reasoning", "두 번째\n\n요약"),
            ("reasoning", "마지막 요약"),
            ("assistant_message", "답변"),
        ]);
    }

    #[test]
    fn omp_thinking_preserves_internal_and_authoritative_unicode_whitespace() {
        let mut mapper = Mapper::new("omp", "omp");
        let mut events = Vec::new();
        assert!(mapper.map(&update("msg-2", 0, "thinking_delta", "\u{2003}")).is_empty());
        events.extend(mapper.map(&update("msg-2", 0, "thinking_delta", "원본🙂e\u{301}")));
        let separator = "\n\t\u{2003}\u{3000}\u{a0}";
        assert!(mapper.map(&update("msg-2", 0, "thinking_delta", separator)).is_empty());
        events.extend(mapper.map(&update("msg-2", 0, "thinking_delta", "다음\u{200b}")));
        assert_eq!(events[1]["item"]["text"], format!("{separator}다음\u{200b}"));
        assert!(mapper.map(&update("msg-2", 0, "thinking_delta", "\n\n")).is_empty());
        let final_body = format!("\u{2003}원본🙂e\u{301}{separator}다음\u{200b}\n\n");
        events.extend(mapper.map(&update("msg-2", 0, "thinking_end", &final_body)));
        assert_eq!(events[2]["item"]["text"], "\n\n");
        assert!(mapper.map(&end("msg-2", json!([
            {"type":"thinking","thinking":final_body}
        ]))).is_empty());
        assert_eq!(rendered(&events, "reasoning"), final_body);
        assert!(events.iter().all(|event| event["item"]["type"] == "reasoning"));
    }

    #[test]
    fn omp_unconfirmed_thinking_tail_blocks_later_content_until_authority() {
        for full_snapshot in [false, true] {
            let mut mapper = Mapper::new("omp", "omp");
            let mut events = mapper.map(&update("msg-2", 0, "thinking_delta", "첫🙂"));
            assert!(mapper.map(&update("msg-2", 0, "thinking_delta", "\n\n")).is_empty());
            let frame = if full_snapshot {
                json!({"type":"message_update","messageId":"msg-2",
                    "message":{"role":"assistant","content":[
                        {"type":"thinking","thinking":"첫🙂\n\n"},{"type":"text","text":"답변"}
                    ]},"assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"답변"}})
            } else {
                assert!(mapper.map(&update("msg-2", 1, "text_delta", "답변")).is_empty());
                json!({"type":"message_update","messageId":"msg-2",
                    "message":{"role":"assistant"},
                    "assistantMessageEvent":{"type":"thinking_end","contentIndex":0}})
            };
            assert!(mapper.map(&frame).is_empty());
            events.extend(mapper.map(&end("msg-2", json!([
                {"type":"thinking","thinking":"첫🙂"},{"type":"text","text":"답변 끝"}
            ]))));
            assert_eq!(rendered(&events, "reasoning"), "첫🙂");
            assert_eq!(rendered(&events, "assistant_message"), "답변 끝");
            assert_eq!(events.len(), 2);
            assert_eq!(events[0]["item"]["type"], "reasoning");
            assert_eq!(events[1]["item"]["type"], "assistant_message");
        }
    }

    #[test]
    fn omp_delta_contentless_end_preserves_stream_and_recovers_terminal_suffix() {
        for kind in ["thinking", "text"] {
            let mut mapper = Mapper::new("omp", "omp");
            mapper.map(&start("msg-2", json!([])));
            let mut events =
                mapper.map(&update("msg-2", 0, &format!("{kind}_delta"), "생각🙂e\u{301}"));
            let block_end = json!({"type":"message_update","messageId":"msg-2",
                "message":{"role":"assistant"},
                "assistantMessageEvent":{"type":format!("{kind}_end"),"contentIndex":0}});
            assert!(mapper.map(&block_end).is_empty());
            let block = if kind == "thinking" {
                json!({"type":"thinking","thinking":"생각🙂e\u{301} 끝"})
            } else {
                json!({"type":"text","text":"생각🙂e\u{301} 끝"})
            };
            events.extend(mapper.map(&end("msg-2", json!([block]))));
            let item_type = if kind == "thinking" {
                "reasoning"
            } else {
                "assistant_message"
            };
            assert_eq!(rendered(&events, item_type), "생각🙂e\u{301} 끝");
            assert!(events.iter().all(|event| event["item"]["type"] == item_type));
        }
    }

    #[test]
    fn omp_thinking_separator_does_not_hide_genuine_authoritative_conflicts() {
        for terminal_only in [false, true] {
            for final_body in ["", "다른 내용", "원"] {
                let mut mapper = Mapper::new("omp", "omp");
                mapper.map(&update("msg-2", 0, "thinking_delta", "원본"));
                assert!(mapper.map(&update("msg-2", 0, "thinking_delta", "\n\n")).is_empty());
                let mut events = if terminal_only {
                    Vec::new()
                } else {
                    mapper.map(&update("msg-2", 0, "thinking_end", final_body))
                };
                events.extend(mapper.map(&end("msg-2", json!([
                    {"type":"thinking","thinking":final_body}
                ]))));
                assert_eq!(events.len(), 1);
                assert_eq!(events[0]["item"]["type"], "error");
            }
        }
    }

    #[test]
    fn omp_delta_conflicting_authority_reports_error_without_duplicate_body() {
        for content in [
            json!([{"type":"text","text":"다른 최종 답변"}]),
            json!([{"type":"text","text":"원"}]),
            json!([]),
        ] {
            let mut mapper = Mapper::new("omp", "omp");
            mapper.begin_turn("turn");
            mapper.map(&start("msg-2", json!([])));
            mapper.map(&update("msg-2", 0, "text_delta", "원본"));
            let events = mapper.map(&end("msg-2", content));
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["item"]["type"], "error");
            assert_eq!(rendered(&events, "assistant_message"), "");
            assert_eq!(
                mapper
                    .map(&json!({"type":"agent_end","messages":[]}))
                    .last()
                    .unwrap()["type"],
                "turn_completed"
            );
        }
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::super::Mapper;
    use serde_json::{Value, json};

    #[test]
    fn omp_delta_interleaved_blocks_wait_for_prior_block_authority() {
        let mut mapper = Mapper::new("omp", "omp");
        let delta = |index, text| {
            json!({"type":"message_update","messageId":"msg-2",
            "message":{"role":"assistant"},
            "assistantMessageEvent":{"type":"text_delta","contentIndex":index,"delta":text}})
        };
        let mut events = mapper.map(&delta(0, "첫"));
        assert!(mapper.map(&delta(1, "다음")).is_empty());
        events.extend(mapper.map(&delta(0, "째")));
        events.extend(
            mapper.map(&json!({"type":"message_update","messageId":"msg-2",
            "message":{"role":"assistant"},
            "assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"첫째 완료"}})),
        );
        events.extend(mapper.map(&json!({"type":"message_end","messageId":"msg-2",
        "message":{"role":"assistant","content":[
            {"type":"text","text":"첫째 완료"},{"type":"text","text":"다음 끝"}
        ]}})));
        let rendered: String = events
            .iter()
            .filter_map(|event| event["item"]["text"].as_str())
            .collect();
        assert_eq!(rendered, "첫째 완료다음 끝");
        assert!(
            events
                .iter()
                .all(|event| event["item"]["type"] == "assistant_message")
        );
        let ids: Vec<&Value> = events
            .iter()
            .map(|event| &event["item"]["messageId"])
            .collect();
        assert!(ids.iter().all(|id| *id == ids[0]));
    }
}

#[cfg(test)]
mod authority_tests {
    use super::super::Mapper;
    use serde_json::json;

    #[test]
    fn omp_delta_terminal_replaces_unpublished_block_fragments() {
        let mut mapper = Mapper::new("omp", "omp");
        let mut events = mapper.map(&json!({"type":"message_update","messageId":"msg-2",
            "message":{"role":"assistant"},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"첫"}}));
        assert!(mapper.map(&json!({"type":"message_update","messageId":"msg-2",
            "message":{"role":"assistant"},"assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"빠진중간"}})).is_empty());
        events.extend(mapper.map(&json!({"type":"message_end","messageId":"msg-2",
        "message":{"role":"assistant","content":[
            {"type":"text","text":"첫째"},{"type":"text","text":"빠진 중간 복구🙂"}
        ]}})));
        let text: String = events
            .iter()
            .filter_map(|event| event["item"]["text"].as_str())
            .collect();
        assert_eq!(text, "첫째빠진 중간 복구🙂");
        assert!(
            events
                .iter()
                .all(|event| event["item"]["type"] == "assistant_message")
        );
    }
}
