//! Standalone std-only MCP fixture, compiled by its integration regression.
//! Keeping this independent of libtest prevents harness banners on MCP stdout.
use std::io::{self, BufRead, Write};

fn receive(input: &mut impl BufRead, expected: &str) {
    let mut line = String::new();
    assert!(input.read_line(&mut line).unwrap() > 0, "unexpected MCP EOF");
    assert!(line.contains(expected), "unexpected MCP message: {line}");
}

fn send(message: &str) {
    let mut output = io::stdout().lock();
    writeln!(output, "{message}").unwrap();
    output.flush().unwrap();
}

fn json_string(text: &str) -> String {
    let mut output = String::from("\"");
    for character in text.chars() {
        match character {
            '\"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if c < ' ' => { use std::fmt::Write; write!(&mut output, "\\u{:04x}", c as u32).unwrap(); }
            c => output.push(c),
        }
    }
    output.push('\"');
    output
}

fn main() {
    let mut input = io::stdin().lock();
    receive(&mut input, "\"method\":\"initialize\"");
    send(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}"#);
    receive(&mut input, "\"method\":\"notifications/initialized\"");
    receive(&mut input, "\"method\":\"tools/list\"");
    send(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"fixture","inputSchema":{"type":"object"}}]}}"#);
    receive(&mut input, "\"id\":2");
    receive(&mut input, "\"id\":3");
    send(r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"second"}]}}"#);
    let text = format!("{}:{}", std::env::current_dir().unwrap().display(), std::env::var("MCP_FIXTURE_ENV").unwrap());
    send(&format!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"content":[{{"type":"text","text":{}}}]}}}}"#, json_string(&text)));
    receive(&mut input, "\"id\":4");
    std::fs::write("waiting", "ready").unwrap();
    let mut cancellation = String::new();
    assert!(input.read_line(&mut cancellation).unwrap() > 0);
    assert!(cancellation.contains("\"method\":\"notifications/cancelled\""));
    assert!(cancellation.contains("\"requestId\":4"));
    std::fs::write("cancelled", "cancelled").unwrap();
    let mut eof = String::new();
    assert_eq!(input.read_line(&mut eof).unwrap(), 0, "expected stdin closure");
}
