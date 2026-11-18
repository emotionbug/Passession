//! Linux X11 fixture for tools/glyph-upload-probe.py.

use makepad_widgets::makepad_draw::text::{font::FontId, fonts::Fonts, loader::FontDefinition};
use makepad_widgets::*;
use std::{cell::RefCell, rc::Rc, sync::Arc};

live_design! {
    use makepad_draw::shader::draw_text::DrawText;
    use link::theme::*;

    App = {{App}} {
        window: { title: "Makepad glyph upload probe", inner_size: vec2(440, 150) }
        pass: { clear_color: #102030 }
        text: <DrawText> {
            text_style: <THEME_FONT_REGULAR> { font_size: 34.0 }
            color: #ffffff
        }
    }
}

app_main!(App);

fn main() {
    app_main();
}

#[derive(Live, LiveHook)]
struct App {
    #[live]
    window: WindowHandle,
    #[live]
    pass: Pass,
    #[live]
    text: DrawText,
    #[live]
    list: DrawList2d,
    #[rust]
    advance: Timer,
    #[rust]
    stage: usize,
    #[rust]
    reference: bool,
}

impl LiveRegister for App {
    fn live_register(cx: &mut Cx) {
        makepad_widgets::live_design(cx);
    }
}

impl AppMain for App {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if matches!(event, Event::Startup) {
            self.reference = std::env::var_os("GLYPH_PROBE_FULL").is_some();
            let emoji_path = std::env::var_os("GLYPH_PROBE_EMOJI")
                .expect("set GLYPH_PROBE_EMOJI to NotoColorEmoji.ttf");
            let emoji_id = FontId::from("glyph-upload-probe-emoji");
            let fonts = cx.get_global::<Rc<RefCell<Fonts>>>().clone();
            let mut fonts = fonts.borrow_mut();
            fonts.define_font(
                emoji_id,
                FontDefinition {
                    data: Arc::new(std::fs::read(emoji_path).unwrap()),
                    index: 0,
                    variations: Vec::new(),
                    ascender_fudge_in_ems: 0.0,
                    descender_fudge_in_ems: 0.0,
                },
            );
            let family_id = self.text.text_style.font_family.font_family_id();
            let mut family = fonts.font_family_definition(family_id).unwrap().clone();
            family.font_ids.insert(0, emoji_id);
            fonts.redefine_font_family(family_id, family);
            self.window.set_pass(cx, &self.pass);
            self.advance = cx.start_timeout(1.8);
            eprintln!("GLYPH_PROBE_STAGE=0");
        } else if self.advance.is_event(event).is_some() {
            self.stage += 1;
            if self.stage == 3 {
                cx.quit();
            } else {
                eprintln!("GLYPH_PROBE_STAGE={}", self.stage);
                cx.redraw_all();
                self.advance = cx.start_timeout(1.8);
            }
        } else if let Event::Draw(event) = event {
            let mut draw = CxDraw::new(cx, event);
            let mut draw_2d = Cx2d::new(&mut draw);
            draw_2d.begin_pass(&self.pass, None);
            self.list.begin_always(&mut draw_2d);
            draw_2d.begin_pass_sized_turtle(Layout::default());
            self.text.draw_abs(&mut draw_2d, dvec2(20.0, 36.0), "g");
            self.text.draw_abs(&mut draw_2d, dvec2(230.0, 36.0), "😀");
            if self.stage >= 1 {
                self.text.draw_abs(&mut draw_2d, dvec2(100.0, 36.0), "Q");
            }
            if self.stage >= 2 {
                self.text.draw_abs(&mut draw_2d, dvec2(320.0, 36.0), "🤓");
            }
            draw_2d.end_pass_sized_turtle();
            self.list.end(&mut draw_2d);
            draw_2d.end_pass(&self.pass);
            drop(draw);

            // CxDraw's destructor has handed the updated CPU atlas pages to
            // the renderer. In the reference run, upload each of them in full.
            if self.reference {
                for texture in self.text.draw_vars.texture_slots.iter().flatten() {
                    match texture.get_format(cx) {
                        TextureFormat::VecRu8 { updated, .. } if !updated.is_empty() => {
                            let data = texture.take_vec_u8(cx);
                            texture.put_back_vec_u8(cx, data, None);
                        }
                        TextureFormat::VecBGRAu8_32 { updated, .. } if !updated.is_empty() => {
                            let data = texture.take_vec_u32(cx);
                            texture.put_back_vec_u32(cx, data, None);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
