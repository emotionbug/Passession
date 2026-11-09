use {
    crate::{
        window::WindowId,
        event::{
            MouseDownEvent,
            MouseUpEvent,
            MouseMoveEvent,
            MouseLeaveEvent,
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
pub enum Win32Event {
    AppGotFocus(WindowId),
    AppLostFocus(WindowId),
    WindowResizeLoopStart(WindowId),
    WindowResizeLoopStop(WindowId),
    WindowGeomChange(WindowGeomChangeEvent),
    WindowClosed(WindowClosedEvent),
    Paint,
    
    MouseDown(MouseDownEvent),
    MouseUp(MouseUpEvent),
    MouseMove(MouseMoveEvent),
    MouseLeave(MouseLeaveEvent),
    Scroll(ScrollEvent),
    
    WindowDragQuery(WindowDragQueryEvent),
    WindowCloseRequested(WindowCloseRequestedEvent),
    TextInput(WindowId, TextInputEvent),
    /// An input-method composition step for the focused editor (`ImeComposer`).
    TextComposition(WindowId, crate::ime_composer::CompositionUpdate),
    Drag(DragEvent),
    Drop(DropEvent),
    DragEnd,
    KeyDown(WindowId, KeyEvent),
    KeyUp(WindowId, KeyEvent),
    TextCopy(WindowId, TextClipboardEvent),
    TextCut(WindowId, TextClipboardEvent),
    Timer(TimerEvent),
    Signal,
}
