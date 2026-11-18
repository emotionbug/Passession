use {
    crate::{
        window::WindowId,
        event::{
            MouseDownEvent,
            MouseUpEvent,
            MouseMoveEvent,
            ScrollEvent,
            WindowGeomChangeEvent,
            WindowDragQueryEvent,
            WindowCloseRequestedEvent,
            WindowClosedEvent,
            TextInputEvent,
            KeyEvent,
            DragEvent,
            DropEvent,
            TextClipboardEvent,
            TimerEvent,
        },
    }
};

#[derive(Debug)]
pub enum XlibEvent {
    AppGotFocus(WindowId),
    AppLostFocus(WindowId),
    WindowGeomChange(WindowGeomChangeEvent),
    Expose(WindowId),
    #[cfg(debug_assertions)]
    DamageOracleTouch(crate::event::TouchUpdateEvent),
    WindowClosed(WindowClosedEvent),
    Paint,
    Wake,
    
    MouseDown(MouseDownEvent),
    MouseUp(MouseUpEvent),
    MouseMove(MouseMoveEvent),
    Scroll(ScrollEvent),
    
    WindowDragQuery(WindowDragQueryEvent),
    WindowCloseRequested(WindowCloseRequestedEvent),
    TextInput(WindowId, TextInputEvent),
    TextPasteShortcut(WindowId, crate::event::TextPasteShortcutEvent),
    Drag(WindowId, DragEvent),
    Drop(WindowId, DropEvent),
    DragEnd,
    KeyDown(WindowId, KeyEvent),
    KeyUp(WindowId, KeyEvent),
    TextCopy(WindowId, TextClipboardEvent),
    TextCut(WindowId, TextClipboardEvent),
    Timer(TimerEvent),
}
