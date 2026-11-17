//! Common parsing policy for chat and file previews.
//!
//! CommonMark cannot close `**word’**suffix`: punctuation before a delimiter
//! requires punctuation/space after it. For unspaced prose, permit unresolved
//! asterisk runs next to Unicode punctuation and a letter. The baseline parser
//! supplies the eligible raw-text ranges; code, HTML, escapes, destinations and
//! already recognized emphasis never participate.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use pulldown_cmark::{BrokenLink, CowStr, Event, Options, Parser, Tag, TagEnd};
use regex::Regex;

static PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{P}$").unwrap());

struct Boundary {
    punctuation: usize,
    width: usize,
    raw: Range<usize>,
}

pub struct Markdown<'a> {
    original: &'a str,
    classified: Cow<'a, str>,
    options: Options,
    links: Vec<(Range<usize>, Tag<'a>)>,
}

impl<'a> Markdown<'a> {
    pub fn new(source: &'a str, options: Options) -> Self {
        let mut boundaries = Vec::new();
        let bytes = source.as_bytes();
        let mut offset = 0;
        while offset < bytes.len() {
            if bytes[offset] != b'*' {
                offset += 1;
                continue;
            }
            let start = offset;
            while offset < bytes.len() && bytes[offset] == b'*' {
                offset += 1;
            }
            // Leave long literal runs and escaped runs to CommonMark.
            let escapes = bytes[..start]
                .iter()
                .rev()
                .take_while(|&&b| b == b'\\')
                .count();
            if offset - start > 3 || escapes % 2 != 0 {
                continue;
            }
            let before = source[..start].chars().next_back();
            let after = source[offset..].chars().next();
            if let (Some(before), Some(after)) = (before, after) {
                if unicode_punctuation(before) && after.is_alphabetic() {
                    boundaries.push(Boundary {
                        punctuation: start - before.len_utf8(),
                        width: before.len_utf8(),
                        raw: start - before.len_utf8()..offset,
                    });
                }
                if before.is_alphabetic() && unicode_punctuation(after) {
                    boundaries.push(Boundary {
                        punctuation: offset,
                        width: after.len_utf8(),
                        raw: start..offset + after.len_utf8(),
                    });
                }
            }
        }
        let mut classified = Cow::Borrowed(source);
        let mut links = Vec::new();
        if !boundaries.is_empty() {
            let mut prose: Vec<Range<usize>> = Vec::new();
            let mut code = false;
            let mut unresolved = Vec::new();
            let mut preserve_unresolved = |link: BrokenLink<'a>| {
                unresolved.push(link.span);
                None::<(CowStr<'a>, CowStr<'a>)>
            };
            for (event, range) in Parser::new_with_broken_link_callback(
                source,
                options,
                Some(&mut preserve_unresolved),
            )
            .into_offset_iter()
            {
                match event {
                    Event::Start(Tag::CodeBlock(_)) => code = true,
                    Event::End(TagEnd::CodeBlock) => code = false,
                    Event::Start(tag @ (Tag::Link { .. } | Tag::Image { .. })) => {
                        links.push((range, tag));
                    }
                    Event::Text(text) if !code && text.as_ref() == &source[range.clone()] => {
                        if let Some(last) = prose.last_mut()
                            && last.end == range.start
                        {
                            last.end = range.end;
                        } else {
                            prose.push(range);
                        }
                    }
                    _ => {}
                }
            }
            for boundary in boundaries {
                // Keep unresolved reference syntax unresolved: a classification
                // letter must never accidentally match another authored label.
                if unresolved
                    .iter()
                    .any(|range| range.start <= boundary.raw.start && range.end >= boundary.raw.end)
                {
                    continue;
                }
                let index = prose.partition_point(|range| range.end <= boundary.raw.start);
                if prose.get(index).is_some_and(|range| {
                    range.start <= boundary.raw.start && range.end >= boundary.raw.end
                }) {
                    // Equal UTF-8 widths keep every parser offset in the original
                    // coordinate space. This is classification only, never display
                    // text: events() restores the original bytes before delivery.
                    let letter = match boundary.width {
                        2 => "é",
                        3 => "한",
                        4 => "𐐀",
                        _ => unreachable!("non-ASCII punctuation"),
                    };
                    classified.to_mut().replace_range(
                        boundary.punctuation..boundary.punctuation + boundary.width,
                        letter,
                    );
                }
            }
        }
        Self {
            original: source,
            classified,
            options,
            links,
        }
    }

    pub fn events(&self) -> impl Iterator<Item = Event<'_>> {
        let resolve = |link: BrokenLink<'_>| {
            self.link_at(&link.span).map(|tag| match tag {
                Tag::Link {
                    dest_url, title, ..
                }
                | Tag::Image {
                    dest_url, title, ..
                } => (
                    CowStr::from(dest_url.as_ref()),
                    CowStr::from(title.as_ref()),
                ),
                _ => unreachable!("stored link tag"),
            })
        };
        Parser::new_with_broken_link_callback(&self.classified, self.options, Some(resolve))
            .into_offset_iter()
            .map(|(event, range)| match event {
                Event::InlineHtml(html) if super::html::is_inline_break(&html) => Event::HardBreak,
                Event::Text(text)
                    if matches!(&self.classified, Cow::Owned(_))
                        && text.as_ref() == &self.classified[range.clone()] =>
                {
                    Event::Text(self.original[range].into())
                }
                Event::Start(tag @ (Tag::Link { .. } | Tag::Image { .. })) => {
                    // A changed shortcut/collapsed label may either stop matching
                    // or collide with a different definition. The baseline span,
                    // not the classification spelling, owns its target and id.
                    Event::Start(match self.link_at(&range) {
                        Some(Tag::Link {
                            link_type,
                            dest_url,
                            title,
                            id,
                        }) => Tag::Link {
                            link_type: *link_type,
                            dest_url: dest_url.as_ref().into(),
                            title: title.as_ref().into(),
                            id: id.as_ref().into(),
                        },
                        Some(Tag::Image {
                            link_type,
                            dest_url,
                            title,
                            id,
                        }) => Tag::Image {
                            link_type: *link_type,
                            dest_url: dest_url.as_ref().into(),
                            title: title.as_ref().into(),
                            id: id.as_ref().into(),
                        },
                        _ => tag,
                    })
                }
                other => other,
            })
    }

    fn link_at(&self, range: &Range<usize>) -> Option<&Tag<'a>> {
        let index = self
            .links
            .partition_point(|(span, _)| span.start < range.start);
        self.links
            .get(index)
            .filter(|(span, _)| span == range)
            .map(|(_, tag)| tag)
    }
}

fn unicode_punctuation(ch: char) -> bool {
    !ch.is_ascii() && PUNCTUATION.is_match(ch.encode_utf8(&mut [0; 4]))
}

/// Same-style nesting must survive the inner closing tag in both consumers.
#[derive(Default)]
pub struct StyleDepth {
    bold: usize,
    italic: usize,
    strike: usize,
}

impl StyleDepth {
    pub fn start(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::Strong => self.bold += 1,
            Tag::Emphasis => self.italic += 1,
            Tag::Strikethrough => self.strike += 1,
            _ => {}
        }
    }

    pub fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            _ => {}
        }
    }

    pub fn flags(&self) -> (bool, bool, bool) {
        (self.bold > 0, self.italic > 0, self.strike > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(events: &[Event<'_>]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Text(text) | Event::Code(text) => Some(text.as_ref()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn commonmark_punctuation_diagnostic_and_multilingual_emphasis() {
        let source = "첨부한 위치대로 **호스트 메뉴 맨 아래에 ‘버전’**을 추가하겠습니다.";
        let standard: Vec<_> = Parser::new(source).collect();
        assert!(
            !standard
                .iter()
                .any(|event| matches!(event, Event::Start(Tag::Strong)))
        );
        for (source, visible) in [
            (
                source,
                "첨부한 위치대로 호스트 메뉴 맨 아래에 ‘버전’을 추가하겠습니다.",
            ),
            ("**「設定」**を開く", "「設定」を開く"),
            ("**«نسخة»**العربية", "«نسخة»العربية"),
            ("**«версия»**доступна", "«версия»доступна"),
            ("**“version”**suffix", "“version”suffix"),
            ("前**「設定」**後", "前「設定」後"),
        ] {
            let markdown = Markdown::new(source, Options::empty());
            let events: Vec<_> = markdown.events().collect();
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Event::Start(Tag::Strong))),
                "{source}"
            );
            assert_eq!(text(&events), visible, "{source}");
        }
    }

    #[test]
    fn preserves_commonmark_literals_and_nonprose() {
        for source in [
            "ordinary **bold** and *italic* and ***both***",
            r"\**‘문자’\**을",
            r"\*‘문자’\*을",
            "`**‘코드’**을`",
            "```md\n**‘코드’**을\n```",
            "    **‘코드’**을\n",
            "[label](https://example.test/**‘경로’**을 \"**‘제목’**을\")",
            "[label][id]\n\n[id]: https://example.test/**‘경로’**을",
            "<span title=\"**‘속성’**을\">text</span>",
            "<div>\n**‘HTML’**을\n</div>",
            "**unclosed’**",
            "****‘literal’****suffix",
            "a_b_c and a**!**b",
            "‘**suffix and prefix**’",
            "&#8217;**suffix",
        ] {
            let markdown = Markdown::new(source, Options::empty());
            assert_eq!(
                markdown.events().collect::<Vec<_>>(),
                Parser::new(source).collect::<Vec<_>>(),
                "{source}"
            );
        }
    }

    #[test]
    fn restores_punctuation_entities_and_nested_styles() {
        let source = "***‘a &amp; b’***을";
        let markdown = Markdown::new(source, Options::empty());
        let events: Vec<_> = markdown.events().collect();
        assert_eq!(text(&events), "‘a & b’을");
        assert!(events.contains(&Event::Start(Tag::Strong)));
        assert!(events.contains(&Event::Start(Tag::Emphasis)));
    }

    #[test]
    fn classification_does_not_create_reference_links_or_change_nested_emphasis() {
        for source in [
            "[**‘버전’**을]\n\n[**‘버전한**을]: https://example.test/wrong\n",
            "[**‘버전’**을][]\n\n[**‘버전한**을]: https://example.test/wrong\n",
            "**outer _inner_ tail** and *outer **inner** tail*",
        ] {
            let markdown = Markdown::new(source, Options::empty());
            assert_eq!(
                markdown.events().collect::<Vec<_>>(),
                Parser::new(source).collect::<Vec<_>>(),
                "{source}",
            );
        }
    }
}
