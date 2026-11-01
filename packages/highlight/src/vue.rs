use arborium::advanced::Span;

/// The upstream Vue scanner stops raw_text at the first `}}`, even inside a
/// JavaScript string or nested object. Repair only those injection boundaries;
/// both the surrounding Vue document and the expressions still use native
/// grammars. Masking preserves byte offsets and prevents expression text from
/// being parsed as HTML, without changing the source supplied to TypeScript.
pub(crate) fn highlight(
    highlighter: &mut arborium::Highlighter,
    source: &str,
) -> Result<Vec<Span>, arborium::Error> {
    let spans = highlighter.highlight_spans("vue", source)?;
    let mut interpolations: Vec<_> = spans
        .iter()
        .filter(|span| {
            matches!(
                span.capture.as_str(),
                "punctuation.special" | "paseo.interpolation"
            ) && source[span.start as usize..span.end as usize].starts_with("{{")
        })
        .map(|span| (span.start as usize, span.end as usize))
        .collect();
    interpolations.sort_unstable_by_key(|&(start, end)| (start, std::cmp::Reverse(end)));
    interpolations.dedup_by_key(|(start, _)| *start);

    let mut repairs = Vec::new();
    let mut covered_until = 0;
    for (start, original_end) in interpolations {
        if start < covered_until {
            continue;
        }
        if let Some(end) = expression_end(source.as_bytes(), start + 2) {
            covered_until = end;
            if end != original_end {
                repairs.push(start..end);
            }
        }
    }
    if repairs.is_empty() {
        return Ok(spans);
    }

    let mut masked = source.as_bytes().to_vec();
    for range in &repairs {
        for byte in &mut masked[range.start + 2..range.end - 2] {
            if !matches!(*byte, b'\r' | b'\n') {
                *byte = b' ';
            }
        }
    }
    let masked = std::str::from_utf8(&masked).expect("complete UTF-8 regions replaced by ASCII");
    let mut spans = highlighter.highlight_spans("vue", masked)?;
    spans.retain(|span| {
        let next = repairs.partition_point(|range| range.end <= span.start as usize);
        repairs
            .get(next)
            .is_none_or(|range| span.end as usize <= range.start)
    });
    for range in repairs {
        for (start, end) in [(range.start, range.start + 2), (range.end - 2, range.end)] {
            spans.push(Span {
                start: start as u32,
                end: end as u32,
                capture: "punctuation.special".into(),
                pattern_index: 0,
            });
        }
        let start = range.start + 2;
        for mut span in highlighter.highlight_spans("typescript", &source[start..range.end - 2])? {
            span.start += start as u32;
            span.end += start as u32;
            spans.push(span);
        }
    }
    Ok(spans)
}

// This scan determines only the host-language delimiter, not token styles. A
// stack distinguishes object braces, template text and ${...} substitutions;
// strings, comments and regular expressions cannot terminate an interpolation.
fn expression_end(source: &[u8], mut offset: usize) -> Option<usize> {
    let mut stack = Vec::new();
    let mut operand = true;
    while offset < source.len() {
        let byte = source[offset];
        if stack.last() == Some(&b'`') {
            match byte {
                b'\\' => offset += 2,
                b'`' => {
                    stack.pop();
                    operand = false;
                    offset += 1;
                }
                b'$' if source.get(offset + 1) == Some(&b'{') => {
                    stack.push(b'$');
                    operand = true;
                    offset += 2;
                }
                _ => offset += 1,
            }
            continue;
        }
        if byte == b'}' && stack.is_empty() && source.get(offset + 1) == Some(&b'}') {
            return Some(offset + 2);
        }
        match byte {
            b'\'' | b'"' => {
                offset = quoted_end(source, offset + 1, byte)?;
                operand = false;
            }
            b'`' => {
                stack.push(b'`');
                offset += 1;
            }
            b'/' if source.get(offset + 1) == Some(&b'/') => {
                offset += 2;
                while offset < source.len() && !matches!(source[offset], b'\r' | b'\n') {
                    offset += 1;
                }
            }
            b'/' if source.get(offset + 1) == Some(&b'*') => {
                offset += 2;
                while offset + 1 < source.len() && &source[offset..offset + 2] != b"*/" {
                    offset += 1;
                }
                offset += 2;
            }
            b'/' if operand => {
                offset = regex_end(source, offset + 1)?;
                operand = false;
            }
            b'{' | b'(' | b'[' => {
                stack.push(byte);
                operand = true;
                offset += 1;
            }
            b'}' | b')' | b']' => {
                stack.pop();
                operand = false;
                offset += 1;
            }
            b if b.is_ascii_whitespace() => offset += 1,
            b if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80 => {
                let start = offset;
                offset += 1;
                while source.get(offset).is_some_and(|b| {
                    b.is_ascii_alphanumeric() || matches!(*b, b'_' | b'$') || *b >= 0x80
                }) {
                    offset += 1;
                }
                operand = matches!(
                    &source[start..offset],
                    b"return"
                        | b"throw"
                        | b"case"
                        | b"delete"
                        | b"void"
                        | b"typeof"
                        | b"instanceof"
                        | b"in"
                        | b"of"
                        | b"yield"
                        | b"await"
                        | b"new"
                );
            }
            b'+' | b'-' if source.get(offset + 1) == Some(&byte) => {
                offset += 2;
            }
            _ => {
                operand = byte != b'.';
                offset += 1;
            }
        }
    }
    None
}

fn quoted_end(source: &[u8], mut offset: usize, quote: u8) -> Option<usize> {
    while offset < source.len() {
        match source[offset] {
            b'\\' => {
                offset += if source.get(offset + 1..offset + 3) == Some(b"\r\n") {
                    3
                } else {
                    2
                };
            }
            byte if byte == quote => return Some(offset + 1),
            b'\r' | b'\n' => return None,
            _ => offset += 1,
        }
    }
    None
}

fn regex_end(source: &[u8], mut offset: usize) -> Option<usize> {
    let mut character_class = false;
    while offset < source.len() {
        match source[offset] {
            b'\\' => offset += 2,
            b'[' => {
                character_class = true;
                offset += 1;
            }
            b']' => {
                character_class = false;
                offset += 1;
            }
            b'/' if !character_class => return Some(offset + 1),
            b'\r' | b'\n' => return None,
            _ => offset += 1,
        }
    }
    None
}
