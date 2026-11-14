use super::*;

live_design! {
    use link::shaders::*;
    ExtendedDrawText = {{ExtendedDrawText}} {
        focus: 0.25,
        hover: 0.75,
    }
}

#[derive(Live, LiveHook, LiveRegister)]
#[repr(C)]
struct ExtendedDrawText {
    #[deref]
    draw_super: DrawText,
    #[live]
    focus: f32,
    #[live]
    hover: f32,
}

#[test]
fn native_text_subclasses_upload_their_authored_instance_values() {
    let mut cx = Cx::new(Box::new(|_, _| {}));
    crate::live_design(&mut cx);
    live_design(&mut cx);
    ExtendedDrawText::register_main_module(&mut cx);
    cx.live_expand();
    let text = ExtendedDrawText::new_local(&mut cx);
    assert!(text.draw_vars.draw_shader.is_some());
    let instances = text.draw_vars.as_slice();
    // Reflection places the derived fields after 19 DrawText floats and
    // one alignment slot, not after any unreflected Rust-only state.
    assert_eq!(instances.len(), 22);
    assert_eq!(instances[20], 0.25);
    assert_eq!(instances[21], 0.75);
}
