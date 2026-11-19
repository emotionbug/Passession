use {
    crate::{
        event::{
            MouseDownEvent,
            MouseUpEvent,
            MouseMoveEvent,
            ScrollEvent,
            WindowGeomChangeEvent,
            TextImeEvent,
            TimerEvent,
            LongPressEvent,
            TouchUpdateEvent,
            VirtualKeyboardEvent,
        },
    },
    std::{cell::Cell, rc::Rc},
};

#[derive(Debug, Clone)]
pub enum IosEvent {
    Init,
    Wake,
    ExecutorTick,
    WindowGeomChange(WindowGeomChangeEvent),
    DisplayFrame,
    SurfaceLayout,
    SurfaceReady,
    SurfaceResumed,
    SurfaceSuspended,
    SurfaceMemoryWarning,
    AccessibilityStatus,
    AccessibilityActivate { id: u64, handled: Rc<Cell<bool>> },
    VirtualKeyboard(VirtualKeyboardEvent),
    MouseDown(MouseDownEvent),
    MouseUp(MouseUpEvent),
    MouseMove(MouseMoveEvent),
    TouchUpdate(TouchUpdateEvent),
    LongPress(LongPressEvent),
    
    Scroll(ScrollEvent),
    
    TextEdit {
        session: u64,
        text: String,
        selection: (usize, usize),
        composing: Option<(usize, usize)>,
        ended: bool,
    },
    TextCompositionEnded(TextImeEvent),
    TextSubmit(u64),
    Timer(TimerEvent),
}
