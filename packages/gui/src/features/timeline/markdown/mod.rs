//! Native Markdown for chat and plan cards: HTML normalization, the block
//! model and the painter. Parses are cached in the row state; a streaming
//! message reuses its stable head (StreamingMarkdown) so only the last block
//! is parsed again for each revealed slice.

pub mod html;
pub mod model;
pub mod view;

use super::paint::Draw;
use makepad_widgets::*;
use model::Block;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;
pub use view::{FindMark, MdOpts};

fn hash(text: &str, linkify: bool, root: Option<&str>) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    linkify.hash(&mut hasher);
    root.hash(&mut hasher);
    hasher.finish()
}

/// Parsed blocks for `text`, cached by content in the row state (bounded).
pub fn blocks(d: &mut Draw, text: &str, root: Option<&str>, linkify: bool) -> Rc<Vec<Block>> {
    let key = hash(text, linkify, root);
    if let Some(hit) = d.state.markdown.get(&key) {
        return hit.clone();
    }
    let parsed = Rc::new(model::parse(text, root, linkify));
    if d.state.markdown.len() > 8 {
        d.state.markdown.clear();
    }
    d.state.markdown.insert(key, parsed.clone());
    parsed
}

/// Live snapshots replace one another; keeping nine ever-growing tails retains
/// their parsed strings and layouts long after they can be drawn again.
fn stream_blocks(d: &mut Draw, text: &str, root: Option<&str>, linkify: bool) -> Rc<Vec<Block>> {
    let key = hash(text, linkify, root);
    if let Some((old, blocks)) = &d.state.markdown_tail
        && *old == key
    {
        return blocks.clone();
    }
    let parsed = Rc::new(model::parse(text, root, linkify));
    d.state.markdown_tail = Some((key, parsed.clone()));
    parsed
}

#[allow(clippy::too_many_arguments)]
fn draw_parsed(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, blocks: &Rc<Vec<Block>>, x: f64, y: f64, w: f64, color: Vec4) -> f64 {
    let previous = d.state.markdown_paragraphs.set_owner(Some(blocks.clone()));
    let height = view::draw_blocks(cx, d, opts, blocks, x, y, w, color, false);
    d.state.markdown_paragraphs.set_owner(previous);
    height
}

/// Draws Markdown `text`; returns its height. `streaming` keeps the stable
/// head parsed once while the tail grows.
#[allow(clippy::too_many_arguments)]
pub fn draw(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, text: &str, x: f64, y: f64, w: f64, linkify: bool, streaming: bool) -> f64 {
    let color = d.look.palette.foreground;
    let root = opts.workspace_root;
    if streaming
        && opts.find.is_none()
        && let Some(split) = model::markdown_stable_split(text)
    {
        let head_text = text[..split].trim_end();
        let head_key = hash(head_text, linkify, root);
        let head = match &d.state.stable_head {
            Some((key, blocks)) if *key == head_key => blocks.clone(),
            _ => {
                let parsed = Rc::new(model::parse(head_text, root, linkify));
                d.state.stable_head = Some((head_key, parsed.clone()));
                parsed
            }
        };
        let tail = stream_blocks(d, &text[split..], root, linkify);
        let mut h = draw_parsed(cx, d, opts, &head, x, y, w, color);
        h += view::block_gap(d.look.compact);
        h += draw_parsed(cx, d, opts, &tail, x, y + h, w, color);
        return h;
    }
    let parsed = if streaming {
        stream_blocks(d, text, root, linkify)
    } else {
        d.state.markdown_tail = None;
        blocks(d, text, root, linkify)
    };
    draw_parsed(cx, d, opts, &parsed, x, y, w, color)
}
