use crate::os::window_state::{PendingWindowState, WindowStateChange};
use {
    std::{
        ffi::OsStr,
        os::windows::ffi::OsStrExt,
        mem,
        sync::atomic::{AtomicU8, Ordering},
        ffi::c_void,
        cell::{
            Cell,
            RefCell,
        },
    },
    crate::{
        log,
        error,
        windows::{
            core::{BOOL, HRESULT},
            core::PCWSTR,
            core::PCSTR,
            //core::IntoParam,
            Win32::{
                UI::{
                    WindowsAndMessaging::{
                        WNDCLASSEXW,
                        PM_REMOVE,
                        LoadIconW,
                        LoadImageW,
                        GetSystemMetrics,
                        SetWindowPos,
                        HICON,
                        IMAGE_ICON,
                        LR_DEFAULTCOLOR,
                        SM_CXSMICON,
                        SM_CYSMICON,
                        SWP_FRAMECHANGED,
                        SWP_NOACTIVATE,
                        SWP_NOMOVE,
                        SWP_NOSIZE,
                        SWP_NOZORDER,
                        RegisterClassExW,
                        IsGUIThread,
                        GetMessageW,
                        TranslateMessage,
                        DispatchMessageW,
                        PeekMessageW,
                        SetTimer,
                        KillTimer,
                        HCURSOR,
                        ICONINFO,
                        GetIconInfo,
                        CreateIconIndirect,
                        DestroyCursor,
                        SetCursor,
                        LoadCursorW,
                        IsProcessDPIAware,
                        IDC_ARROW,
                        IDC_CROSS,
                        IDC_HAND,
                        IDC_SIZEALL,
                        IDC_IBEAM,
                        IDC_HELP,
                        IDC_NO,
                        IDC_SIZEWE,
                        IDC_SIZENS,
                        IDC_SIZENESW,
                        IDC_SIZENWSE,
                        CS_HREDRAW,
                        CS_VREDRAW,
                        CS_OWNDC,
                        IDI_WINLOGO,
                    },
                    HiDpi::{
                        PROCESS_DPI_AWARENESS,
                        DPI_AWARENESS_CONTEXT,
                        MONITOR_DPI_TYPE,
                        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
                        PROCESS_PER_MONITOR_DPI_AWARE,
                        MDT_EFFECTIVE_DPI
                    },
                },
                Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE},
                Graphics::Gdi::{
                    CreateSolidBrush,
                    HMONITOR,
                    GetDC,
                    MonitorFromWindow,
                    GetDeviceCaps,
                    MONITOR_DEFAULTTONEAREST,
                    LOGPIXELSX,
                    ReleaseDC,
                    HBITMAP,
                    BITMAP,
                    BITMAPINFO,
                    BITMAPINFOHEADER,
                    BI_RGB,
                    DIB_RGB_COLORS,
                    GetDIBits,
                    GetObjectW,
                    CreateDIBSection,
                    CreateBitmap,
                    DeleteObject,
                },
                Foundation::{
                    COLORREF,
                    S_OK,
                    HWND,
                    HINSTANCE,
                    FARPROC,
                    DRAGDROP_S_DROP,
                    DRAGDROP_S_CANCEL,
                },
                System::{
                    Threading::ExitProcess,
                    LibraryLoader::{
                        GetModuleHandleW,
                        LoadLibraryA,
                        GetProcAddress,
                    },
                    Performance::{
                        QueryPerformanceCounter,
                        QueryPerformanceFrequency,
                    },
                    Com::IDataObject,
                    Ole::{
                        OleInitialize,
                        DoDragDrop,
                        IDropSource,
                        DROPEFFECT,
                        DROPEFFECT_COPY,
                        DROPEFFECT_MOVE,
                    },
                },
            },
        },
        event::*,
        cursor::MouseCursor,
        os::{
            cx_native::EventFlow,
            windows::{
                dropsource::*,
                dataobject::DragItemWindows,
                win32_event::Win32Event,
                win32_window::Win32Window,
            },
        },
        window::WindowId,
    },
};
pub const FALSE: BOOL = BOOL(0);

/// Paseo: the caption's light/dark appearance (`Cx::set_window_dark_appearance`):
/// 0 until the app picks a theme (the system's light caption), 1 light, 2 dark.
static DARK_CAPTION: AtomicU8 = AtomicU8::new(0);

/// Applies the chosen caption appearance to one window; failures (Windows before 10 20H1)
/// keep the default caption.
pub fn apply_dark_caption(hwnd: HWND) {
    let mode = DARK_CAPTION.load(Ordering::Relaxed);
    if mode == 0 {
        return;
    }
    let dark = BOOL::from(mode == 2);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const BOOL as *const c_void,
            mem::size_of::<BOOL>() as u32,
        );
    }
}

/// Every window's caption follows the app theme; later windows take it at creation.
pub fn set_dark_appearance(dark: bool) {
    let mode = if dark { 2 } else { 1 };
    if DARK_CAPTION.swap(mode, Ordering::Relaxed) == mode {
        return;
    }
    let windows = WIN32_APP.with_borrow(|app| app.as_ref().map(|app| app.all_windows.clone()).unwrap_or_default());
    for hwnd in windows {
        apply_dark_caption(hwnd);
        // Windows 10 repaints the caption only once its frame changes.
        unsafe {
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
    // Paseo: the cursor's tone follows the theme at once, without waiting for a shape change.
    WIN32_APP.with(|app| {
        if let Ok(mut app) = app.try_borrow_mut() {
            if let Some(app) = app.as_mut() {
                app.retone_mouse_cursor();
            }
        }
    });
}
pub const TRUE: BOOL = BOOL(1);


thread_local! {
    pub static WIN32_APP: RefCell<Option<Win32App>> = RefCell::new(None);
}

pub fn with_win32_app<R>(f: impl FnOnce(&mut Win32App) -> R) -> R {
    WIN32_APP.with_borrow_mut(|app| {
        f(app.as_mut().unwrap())
    })
}

/// The live top-level HWND for an app window, on its UI thread.
pub fn window_hwnd(window: WindowId) -> Option<HWND> {
    WIN32_APP.with_borrow(|app| {
        app.as_ref()?.window_handles.iter().find_map(|&(id, hwnd)| (id == window).then_some(hwnd))
    })
}

pub fn init_win32_app_global(event_callback: Box<dyn FnMut(Win32Event) -> EventFlow>) {
    WIN32_APP.with(|app| {
        *app.borrow_mut() = Some(Win32App::new(event_callback));
    });
}
/*
// copied from Microsoft so it refers to the right IDataObject
#[allow(non_snake_case)]
pub unsafe fn DoDragDrop<P0, P1>(pdataobj: P0, pdropsource: P1, dwokeffects: DROPEFFECT, pdweffect: *mut DROPEFFECT) -> HRESULT
where
P0: IntoParam<IDataObject>,
P1: IntoParam<IDropSource>,
{
    ::windows_targets::link!("ole32.dll" "system" fn DoDragDrop(pdataobj: *mut::core::ffi::c_void, pdropsource: *mut::core::ffi::c_void, dwokeffects: DROPEFFECT, pdweffect: *mut DROPEFFECT) -> HRESULT);
    DoDragDrop(pdataobj.into_param().abi(), pdropsource.into_param().abi(), dwokeffects, pdweffect)
}*/

pub struct Win32App {
    event_callback: Option<Box<dyn FnMut(Win32Event) -> EventFlow >>,
    pending_events: PendingWindowState,
    pub window_class_name: Vec<u16>,
    pub all_windows: Vec<HWND>,
    pub(super) window_handles: Vec<(WindowId, HWND)>,
    pub time: Win32Time,
    pub timers: Vec<Win32Timer>,
    pub was_signal_poll: bool,
    pub event_flow: EventFlow,
    pub dpi_functions: DpiFunctions,
    pub current_cursor: Option<MouseCursor>,
    /// Paseo: the cursor handle on screen for `current_cursor` and the theme tone.
    shown_cursor: Option<ShownCursor>,
    /// Paseo: the pointer left the windows (`WM_MOUSELEAVE`) and no `WM_SETCURSOR` came since.
    pointer_outside: bool,
    pub currently_clicked_window_id: Option<WindowId >,
    pub start_dragging_items: Option<Vec<DragItem >>,
    pub is_dragging_internal: Cell<bool>,
}

#[derive(Clone)]
pub enum Win32Timer {
    Free,
    Timer {win32_id: usize, timer_id: u64, interval: f64, repeats: bool},
    Resize {win32_id: usize},
    DragDrop {win32_id: usize},
    SignalPoll {win32_id: usize},
}

pub struct Win32Time{
    pub time_start: i64,
    pub time_freq: i64,
}

impl Win32Time{
    pub fn new()->Self{
        let mut time_start = 0i64;
        unsafe {QueryPerformanceCounter(&mut time_start).unwrap()};
                
        let mut time_freq = 0i64;
        unsafe {QueryPerformanceFrequency(&mut time_freq).unwrap()};
        Self{
            time_start,
            time_freq,
        }
    }
    
    pub fn time_now(&self)->f64{
        unsafe {
            let mut time_now = 0i64;
            QueryPerformanceCounter(&mut time_now).unwrap();
            (time_now - self.time_start) as f64 / self.time_freq as f64
        }
    }
}

impl Win32App {
    pub fn new(event_callback: Box<dyn FnMut(Win32Event) -> EventFlow>) -> Win32App {
        
        let window_class_name = encode_wide("MakepadWindow\0");
        let instance: HINSTANCE = unsafe {GetModuleHandleW(None).unwrap().into()};
        // Paseo: the executable's icon resource 1 labels the caption, taskbar and Alt+Tab.
        // Upstream used the stock Windows logo, which stays the fallback without one.
        let app_icon = PCWSTR(1 as *const u16);
        let icon = unsafe {LoadIconW(Some(instance), app_icon).or_else(|_| LoadIconW(None, IDI_WINLOGO)).unwrap()};
        let small_icon = unsafe {
            LoadImageW(Some(instance), app_icon, IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR)
                .map(|handle| HICON(handle.0))
                .unwrap_or(icon)
        };
        let class = WNDCLASSEXW {
            cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW
                | CS_VREDRAW
                | CS_OWNDC,
            lpfnWndProc: Some(Win32Window::window_class_proc),
            hInstance: instance,
            hIcon: icon,
            hIconSm: small_icon,
            lpszClassName: PCWSTR(window_class_name.as_ptr()),
            hbrBackground: unsafe {CreateSolidBrush(COLORREF(0x3f3f3f3f))},
            // hCursor stays null so the app's cursor state is not overridden in the client area.
            ..Default::default()
        };
        
        unsafe {
            RegisterClassExW(&class);
            let _ = IsGUIThread(true);
            
            // initialize COM using OleInitialize to allow Drag&Drop and other shell features
            OleInitialize(None).unwrap();
        }
        
        
        let win32_app = Win32App {
            start_dragging_items: None,
            window_class_name,
            was_signal_poll: false,
            time: Win32Time::new(),
            event_callback: Some(event_callback),
            pending_events: PendingWindowState::default(),
            event_flow: EventFlow::Poll,
            all_windows: Vec::new(),
            window_handles: Vec::new(),
            timers: Vec::new(),
            dpi_functions: DpiFunctions::new(),
            current_cursor: None,
            shown_cursor: None,
            pointer_outside: true,
            currently_clicked_window_id: None,
            is_dragging_internal: Cell::new(false),
        };
        win32_app.dpi_functions.become_dpi_aware();
        
        win32_app
    }
    
    pub fn event_loop() {
        unsafe {
            loop {
                let event_flow = with_win32_app(|app| app.event_flow.clone());
                match event_flow {
                    EventFlow::Wait => {
                        let mut msg = std::mem::MaybeUninit::uninit();
                        let ret = GetMessageW(msg.as_mut_ptr(), None, 0, 0);
                        if ret.0 <= 0 {
                            break;
                        }
                        else {
                            let msg = msg.assume_init();
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                            if !with_win32_app(|app| app.was_signal_poll()) {
                                Win32App::do_callback(Win32Event::Paint);
                            }
                        }
                    }
                    EventFlow::Poll => {
                        let mut msg = std::mem::MaybeUninit::uninit();
                        let ret = PeekMessageW(msg.as_mut_ptr(), None, 0, 0, PM_REMOVE);
                        if ret == FALSE {
                            Win32App::do_callback(Win32Event::Paint)
                        }
                        else {
                            let msg = msg.assume_init();
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                    EventFlow::Exit => break,
                }
                Win32App::poll_start_drag_drop();
            }
        }
    }
    
    pub fn do_callback(event: Win32Event) {
        let cb = with_win32_app(|app| app.event_callback.take());
        if let Some(mut callback) = cb {
            let mut event = event;
            loop {
                let event_flow = callback(event);
                with_win32_app(|app| app.event_flow = event_flow);
                if let EventFlow::Exit = event_flow {
                    unsafe {ExitProcess(0);}
                }
                // Keep the callback taken while draining: nested notifications append
                // behind earlier ones, including those from platform ops.
                if let Some(pending) = with_win32_app(|app| app.pending_events.pop_front()) {
                    event = match pending {
                        WindowStateChange::Focus(window_id, true) => Win32Event::AppGotFocus(window_id),
                        WindowStateChange::Focus(window_id, false) => Win32Event::AppLostFocus(window_id),
                        WindowStateChange::Geometry(change) => Win32Event::WindowGeomChange(change),
                    };
                } else {
                    break;
                }
            }
            with_win32_app(|app| app.event_callback = Some(callback));
        } else {
            // Paseo: the callback is running and this message was sent from inside it
            // (a platform op's MoveWindow/ShowWindow, a DPI change, a modal loop).
            with_win32_app(|app| match event {
                Win32Event::AppGotFocus(window_id) => app.pending_events.focus(window_id, true),
                Win32Event::AppLostFocus(window_id) => app.pending_events.focus(window_id, false),
                Win32Event::WindowGeomChange(change) => app.pending_events.geometry(change),
                _ => (),
            });
        }
    }
    
    pub unsafe extern "system" fn timer_proc(_hwnd: HWND, _arg1: u32, in_win32_id: usize, _arg2: u32) {
        let hit_timer = {
            with_win32_app(|app|{
                let mut hit_timer = None;
                for slot in 0..app.timers.len() {
                    match app.timers[slot] {
                        Win32Timer::Timer {win32_id, repeats, ..} => if win32_id == in_win32_id {
                            hit_timer = Some(app.timers[slot].clone());
                            if !repeats {
                                KillTimer(None, in_win32_id).unwrap();
                                app.timers[slot] = Win32Timer::Free;
                            }
                            break;
                        },
                        Win32Timer::DragDrop {win32_id, ..} => if win32_id == in_win32_id {
                            hit_timer = Some(app.timers[slot].clone());
                            break;
                        },
                        Win32Timer::Resize {win32_id, ..} => if win32_id == in_win32_id {
                            hit_timer = Some(app.timers[slot].clone());
                            break;
                        },
                        Win32Timer::SignalPoll {win32_id, ..} => if win32_id == in_win32_id {
                            hit_timer = Some(app.timers[slot].clone());
                            break;
                        }
                        _ => ()
                    }
                };
                hit_timer
            })
        };
        // call the dependencies
        let time =  with_win32_app(|app| app.time_now());
        if let Some(hit_timer) = hit_timer {
            match hit_timer {
                Win32Timer::Timer {timer_id, ..} => {
                    Win32App::do_callback(Win32Event::Timer(TimerEvent {
                        time: Some(time),
                        timer_id: timer_id
                    }));
                },
                Win32Timer::Resize {..} => {
                    Win32App::do_callback(Win32Event::Paint);
                },
                Win32Timer::DragDrop {..} => {
                    Win32App::do_callback(Win32Event::Paint);
                },
                Win32Timer::SignalPoll {..} => {
                    Win32App::do_callback(
                        Win32Event::Signal
                    );
                    with_win32_app(|app| app.was_signal_poll = true);
                }
                _ => ()
            }
        }
    }
    
    pub fn was_signal_poll(&mut self) -> bool {
        if self.was_signal_poll {
            self.was_signal_poll = false;
            true
        }
        else {
            false
        }
    }
    
    pub fn get_free_timer_slot(&mut self) -> usize {
        //let win32_app = get_win32_app_global();
        for slot in 0..self.timers.len() {
            if let Win32Timer::Free = self.timers[slot] {
                return slot
            }
        }
        let slot = self.timers.len();
        self.timers.push(Win32Timer::Free);
        slot
    }
    
    pub fn start_timer(&mut self, timer_id: u64, interval: f64, repeats: bool) {
        let slot = self.get_free_timer_slot();
        let win32_id = unsafe {SetTimer(None, 0, (interval * 1000.0) as u32, Some(Self::timer_proc))};
        if timer_id == 0{
            self.timers[slot] = Win32Timer::SignalPoll {
                win32_id: win32_id,
            };
        }
        else{
            self.timers[slot] = Win32Timer::Timer {
                timer_id: timer_id,
                win32_id: win32_id,
                interval: interval,
                repeats: repeats
            };
        }
    }
    
    pub fn stop_timer(&mut self, which_timer_id: u64) {
        for slot in 0..self.timers.len() {
            if let Win32Timer::Timer {win32_id, timer_id, ..} = self.timers[slot] {
                if timer_id == which_timer_id {
                    self.timers[slot] = Win32Timer::Free;
                    unsafe {KillTimer(None, win32_id).unwrap();}
                }
            }
        }
    }
    
    pub fn start_resize(&mut self) {
        let slot = self.get_free_timer_slot();
        let win32_id = unsafe {SetTimer(None, 0, 8 as u32, Some(Self::timer_proc))};
        self.timers[slot] = Win32Timer::Resize {win32_id: win32_id};
    }
    
    pub fn poll_start_drag_drop() {
        let items = with_win32_app(|app| app.start_dragging_items.take());
        if let Some(items) = items {
            with_win32_app(|app| {
                let slot = app.get_free_timer_slot();
                let win32_id = unsafe {SetTimer(None, 0, 8 as u32, Some(Self::timer_proc))};
                app.timers[slot] = Win32Timer::DragDrop {win32_id: win32_id};
            });
            
            if items.len() > 1 {
                error!("multi-item drag/drop operation not supported");
            }
            match &items[0] {
                DragItem::FilePath {path, internal_id,} => {
                    
                    //log!("win32: about to drag path \"{}\" with internal ID {:?}", path, internal_id);
                    
                    // only drag if something is there
                    if (path.len() > 0) || internal_id.is_some() {
                        
                        // create COM IDataObject that hosts the drag item
                        let data_object: IDataObject = DragItemWindows(DragItem::FilePath {path: path.clone(), internal_id: internal_id.clone(),}).into();
                        
                        // create COM IDropSource to indicate when to stop dragging
                        let drop_source: IDropSource = DropSource {}.into();
                        
                        with_win32_app(|app| app.is_dragging_internal.replace(true));
                        let mut effect = DROPEFFECT(0);
                        match unsafe {DoDragDrop(&data_object, &drop_source, DROPEFFECT_COPY | DROPEFFECT_MOVE, &mut effect)} {
                            DRAGDROP_S_DROP => {/*log!("DoDragDrop: succesful")*/},
                            DRAGDROP_S_CANCEL => {/*log!("DoDragDrop: canceled")*/},
                            _ => {log!("DoDragDrop: failed for some reason")},
                        }
                        with_win32_app(|app| app.is_dragging_internal.replace(false));
                    }
                },
                _ => {
                    error!("Only DragItem::FilePath supported");
                }
            }
            with_win32_app(|app|{
                for slot in 0..app.timers.len() {
                    if let Win32Timer::DragDrop {win32_id} = app.timers[slot] {
                        app.timers[slot] = Win32Timer::Free;
                        unsafe {KillTimer(None, win32_id).unwrap();}
                    }
                }
            })
        }
    }
    
    pub fn start_signal_poll(&mut self) {
        let slot = self.get_free_timer_slot();
        let win32_id = unsafe {SetTimer(None, 0, 8 as u32, Some(Self::timer_proc))};
        self.timers[slot] = Win32Timer::SignalPoll {win32_id: win32_id};
    }
    
    pub fn stop_resize(&mut self) {
        for slot in 0..self.timers.len() {
            if let Win32Timer::Resize {win32_id} = self.timers[slot] {
                self.timers[slot] = Win32Timer::Free;
                unsafe {KillTimer(None, win32_id).unwrap();}
            }
        }
    }
    
    pub fn start_dragging(&mut self, items: Vec<DragItem>) {
        self.start_dragging_items = Some(items);
    }
    
    pub fn time_now(&self) -> f64 {
        self.time.time_now()
    }
    
    pub fn set_mouse_cursor(&mut self, cursor: MouseCursor) {
        if self.current_cursor != Some(cursor) {
            self.current_cursor = Some(cursor);
            // Outside the windows the shape waits for `WM_SETCURSOR`; setting it now would
            // change the pointer over another application.
            if !self.pointer_outside {
                self.apply_mouse_cursor();
            }
        }
    }

    /// Paseo: re-sets the app's cursor after the native frame showed its own (`WM_SETCURSOR`
    /// over the client area).
    pub fn reapply_mouse_cursor(&mut self) {
        self.pointer_outside = false;
        self.current_cursor.get_or_insert(MouseCursor::Default);
        self.apply_mouse_cursor();
    }

    /// Paseo: `WM_MOUSELEAVE`; upstream recorded `Hidden` here to force the next shape,
    /// which `reapply_mouse_cursor` then showed as no pointer at all on re-entry.
    pub fn mouse_cursor_left(&mut self) {
        self.pointer_outside = true;
    }

    /// Paseo: shows `current_cursor` in the theme's tone. `Hidden` is a null cursor rather
    /// than `ShowCursor`, whose display count upstream raised on every shape change, so
    /// hiding never took effect again.
    fn apply_mouse_cursor(&mut self) {
        let cursor = self.current_cursor.unwrap_or(MouseCursor::Default);
        let id = win32_cursor_id(cursor);
        unsafe {
            if id == PCWSTR::null() {
                SetCursor(None);
                return;
            }
            let tone = DARK_CAPTION.load(Ordering::Relaxed);
            if let Some(shown) = self.shown_cursor.as_ref().filter(|s| s.cursor == cursor && s.tone == tone) {
                SetCursor(Some(shown.handle));
                return;
            }
            let Ok(system) = LoadCursorW(None, id) else { return };
            let toned = if tone == 0 { None } else { toned_system_cursor(system, tone == 2) };
            let shown = ShownCursor { cursor, tone, handle: toned.unwrap_or(system), owned: toned.is_some() };
            SetCursor(Some(shown.handle));
            if let Some(previous) = self.shown_cursor.replace(shown) {
                if previous.owned {
                    let _ = DestroyCursor(previous.handle);
                }
            }
        }
    }
}

/// Paseo: a system cursor as shown, possibly a recoloured copy this app owns.
struct ShownCursor {
    cursor: MouseCursor,
    tone: u8,
    handle: HCURSOR,
    owned: bool,
}

/// Paseo: a copy of a system cursor recoloured to contrast with the theme
/// (`cursor_tone`), or `None` when it already contrasts or cannot be read. The copy keeps
/// the system image's size, so the user's pointer size and the system scaling apply.
unsafe fn toned_system_cursor(system: HCURSOR, dark_theme: bool) -> Option<HCURSOR> {
    let mut info = ICONINFO::default();
    unsafe {
        GetIconInfo(system.into(), &mut info).ok()?;
        let toned = toned_cursor_from(&info, dark_theme);
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor.into());
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask.into());
        }
        toned
    }
}

unsafe fn toned_cursor_from(info: &ICONINFO, dark_theme: bool) -> Option<HCURSOR> {
    // Read as 32 bits per pixel, a set mask bit is white.
    let set = |pixel: u32| pixel & 0x00ff_ffff != 0;
    let (width, mask_height, mask) = unsafe { bitmap_pixels(info.hbmMask)? };
    let (height, mut pixels) = if info.hbmColor.is_invalid() {
        // Monochrome: the AND mask stacked over the XOR mask.
        let height = mask_height / 2;
        let mut pixels = Vec::with_capacity(width * height);
        for i in 0..width * height {
            pixels.push(match (set(mask[i]), set(mask[width * height + i])) {
                (false, false) => 0xff00_0000,
                (false, true) => 0xffff_ffff,
                (true, false) => 0,
                // Inverts the screen beneath, which contrasts with any theme.
                (true, true) => return None,
            });
        }
        (height, pixels)
    } else {
        let (color_width, height, mut pixels) = unsafe { bitmap_pixels(info.hbmColor)? };
        if color_width != width || pixels.len() > mask.len() {
            return None;
        }
        if pixels.iter().all(|pixel| pixel >> 24 == 0) {
            // No alpha channel: the AND mask marks the transparent pixels.
            for (pixel, &bit) in pixels.iter_mut().zip(&mask) {
                if !set(bit) {
                    *pixel |= 0xff00_0000;
                } else if *pixel & 0x00ff_ffff != 0 {
                    return None;
                }
            }
        }
        (height, pixels)
    };
    if !crate::cursor_tone::tone_for_theme(&mut pixels, width, height, crate::cursor_tone::PixelAlpha::Straight, dark_theme) {
        return None;
    }
    unsafe { create_cursor(width, height, &pixels, info.xHotspot, info.yHotspot) }
}

fn dib_info(width: usize, height: usize) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            // Negative: rows run top to bottom.
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A bitmap's pixels as top-down 0xAARRGGBB.
unsafe fn bitmap_pixels(bitmap: HBITMAP) -> Option<(usize, usize, Vec<u32>)> {
    let mut header = BITMAP::default();
    let size = mem::size_of::<BITMAP>() as i32;
    if unsafe { GetObjectW(bitmap.into(), size, Some(&mut header as *mut BITMAP as *mut c_void)) } == 0 {
        return None;
    }
    if header.bmWidth <= 0 || header.bmHeight <= 0 {
        return None;
    }
    let (width, height) = (header.bmWidth as usize, header.bmHeight as usize);
    let mut info = dib_info(width, height);
    let mut pixels = vec![0u32; width * height];
    let rows = unsafe {
        let dc = GetDC(None);
        let rows = GetDIBits(dc, bitmap, 0, height as u32, Some(pixels.as_mut_ptr() as *mut c_void), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        rows
    };
    (rows == height as i32).then_some((width, height, pixels))
}

unsafe fn create_cursor(width: usize, height: usize, pixels: &[u32], x_hotspot: u32, y_hotspot: u32) -> Option<HCURSOR> {
    let info = dib_info(width, height);
    let mut bits: *mut c_void = std::ptr::null_mut();
    unsafe {
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u32, pixels.len());
        // Windows draws by the alpha channel; the AND mask matches it for anything that
        // reads the mask instead (set bits are transparent; rows are word aligned).
        let stride = width.div_ceil(16) * 2;
        let mut mask_bits = vec![0u8; stride * height];
        for (i, pixel) in pixels.iter().enumerate() {
            if pixel >> 24 < 0x80 {
                let (x, y) = (i % width, i / width);
                mask_bits[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
        let mask = CreateBitmap(width as i32, height as i32, 1, 1, Some(mask_bits.as_ptr() as *const c_void));
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: FALSE,
            xHotspot: x_hotspot,
            yHotspot: y_hotspot,
            hbmMask: mask,
            hbmColor: color,
        });
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon.ok().map(|icon| HCURSOR(icon.0))
    }
}

impl Win32App {
    /// Paseo: a theme change re-tones the cursor at once while the pointer is over a window.
    fn retone_mouse_cursor(&mut self) {
        if !self.pointer_outside {
            self.apply_mouse_cursor();
        }
    }
}

fn win32_cursor_id(cursor: MouseCursor) -> PCWSTR {
    match cursor {
        MouseCursor::Hidden => PCWSTR::null(),
        MouseCursor::Default => IDC_ARROW,
        MouseCursor::Crosshair => IDC_CROSS,
        MouseCursor::Hand => IDC_HAND,
        // Default to Hand for non-supported cursors, until we include our own custom cursor files.
        MouseCursor::Grab | MouseCursor::Grabbing => IDC_HAND,
        MouseCursor::Arrow => IDC_ARROW,
        MouseCursor::Move => IDC_SIZEALL,
        MouseCursor::Text => IDC_IBEAM,
        MouseCursor::Wait => IDC_ARROW,
        MouseCursor::Help => IDC_HELP,
        MouseCursor::NotAllowed => IDC_NO,

        MouseCursor::EResize => IDC_SIZEWE,
        MouseCursor::NResize => IDC_SIZENS,
        MouseCursor::NeResize => IDC_SIZENESW,
        MouseCursor::NwResize => IDC_SIZENWSE,
        MouseCursor::SResize => IDC_SIZENS,
        MouseCursor::SeResize => IDC_SIZENWSE,
        MouseCursor::SwResize => IDC_SIZENESW,
        MouseCursor::WResize => IDC_SIZEWE,

        MouseCursor::NsResize => IDC_SIZENS,
        MouseCursor::NeswResize => IDC_SIZENESW,
        MouseCursor::EwResize => IDC_SIZEWE,
        MouseCursor::NwseResize => IDC_SIZENWSE,

        MouseCursor::ColResize => IDC_SIZEWE,
        MouseCursor::RowResize => IDC_SIZENS,
    }
}



// reworked from winit windows platform https://github.com/rust-windowing/winit/blob/eventloop-2.0/src/platform_impl/windows/dpi.rs

type SetProcessDPIAware = unsafe extern "system" fn () -> BOOL;
type SetProcessDpiAwareness = unsafe extern "system" fn (value: PROCESS_DPI_AWARENESS,) -> HRESULT;
type SetProcessDpiAwarenessContext = unsafe extern "system" fn (value: DPI_AWARENESS_CONTEXT,) -> BOOL;
type GetDpiForWindow = unsafe extern "system" fn (hwnd: HWND) -> u32;
type GetDpiForMonitor = unsafe extern "system" fn (hmonitor: HMONITOR, dpi_type: MONITOR_DPI_TYPE, dpi_x: *mut u32, dpi_y: *mut u32) -> HRESULT;
type EnableNonClientDpiScaling = unsafe extern "system" fn (hwnd: HWND) -> BOOL;

// Helper function to dynamically load function pointer.
// `library` and `function` must be zero-terminated.
fn get_function_impl(library: &str, function: &str) -> FARPROC {
    // Library names we will use are ASCII so we can use the A version to avoid string conversion.
    
    let module = unsafe {LoadLibraryA(PCSTR::from_raw(library.as_ptr()))};
    if module.is_err() {
        return None;
    }
    
    let function_ptr = unsafe {GetProcAddress(module.unwrap(), PCSTR::from_raw(function.as_ptr()))};
    if function_ptr.is_none() {
        return None;
    }
    
    function_ptr
}

macro_rules!get_function {
    ( $ lib: expr, $ func: ident) => {
        get_function_impl(concat!( $ lib, '\0'), concat!(stringify!( $ func), '\0'))
            .map( | f | unsafe {mem::transmute::<_, $ func>(f)})
    }
}

pub fn encode_wide(string: impl AsRef<OsStr>) -> Vec<u16> {
    string.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/*
pub fn post_signal_to_hwnd(hwnd:HWND, signal:Signal){
    unsafe{PostMessageW(
        hwnd,
        WM_USER,
        WPARAM(((signal.0.0)&0xffff_ffff) as usize),
        LPARAM(((signal.0.0>>32)&0xffff_ffff) as isize),
    )};
}
*/
pub struct DpiFunctions {
    get_dpi_for_window: Option<GetDpiForWindow>,
    get_dpi_for_monitor: Option<GetDpiForMonitor>,
    enable_nonclient_dpi_scaling: Option<EnableNonClientDpiScaling>,
    set_process_dpi_awareness_context: Option<SetProcessDpiAwarenessContext>,
    set_process_dpi_awareness: Option<SetProcessDpiAwareness>,
    set_process_dpi_aware: Option<SetProcessDPIAware>
}

const BASE_DPI: u32 = 96;

impl DpiFunctions {
    fn new() -> DpiFunctions {
        DpiFunctions {
            get_dpi_for_window: get_function!("user32.dll", GetDpiForWindow),
            get_dpi_for_monitor: get_function!("shcore.dll", GetDpiForMonitor),
            enable_nonclient_dpi_scaling: get_function!("user32.dll", EnableNonClientDpiScaling),
            set_process_dpi_awareness_context: get_function!("user32.dll", SetProcessDpiAwarenessContext),
            set_process_dpi_awareness: get_function!("shcore.dll", SetProcessDpiAwareness),
            set_process_dpi_aware: get_function!("user32.dll", SetProcessDPIAware)
        }
    }
    
    fn become_dpi_aware(&self) {
        unsafe {
            if let Some(set_process_dpi_awareness_context) = self.set_process_dpi_awareness_context {
                // We are on Windows 10 Anniversary Update (1607) or later.
                if set_process_dpi_awareness_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == FALSE {
                    // V2 only works with Windows 10 Creators Update (1703). Try using the older
                    // V1 if we can't set V2.
                    let _ = set_process_dpi_awareness_context(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
                }
            }
            else if let Some(set_process_dpi_awareness) = self.set_process_dpi_awareness {
                // We are on Windows 8.1 or later.
                set_process_dpi_awareness(PROCESS_PER_MONITOR_DPI_AWARE).unwrap();
            }
            else if let Some(set_process_dpi_aware) = self.set_process_dpi_aware {
                // We are on Vista or later.
                set_process_dpi_aware().unwrap();
            }
        }
    }
    
    pub fn enable_non_client_dpi_scaling(&self, hwnd: HWND) {
        unsafe {
            if let Some(enable_nonclient_dpi_scaling) = self.enable_nonclient_dpi_scaling {
                let _ = enable_nonclient_dpi_scaling(hwnd);
            }
        }
    }
    /*
    pub fn get_monitor_dpi(hmonitor: HMONITOR) -> Option<u32> {
        unsafe {
            if let Some(GetDpiForMonitor) = *GET_DPI_FOR_MONITOR {
                // We are on Windows 8.1 or later.
                let mut dpi_x = 0;
                let mut dpi_y = 0;
                if GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == S_OK {
                    // MSDN says that "the values of *dpiX and *dpiY are identical. You only need to
                    // record one of the values to determine the DPI and respond appropriately".
                    // https://msdn.microsoft.com/en-us/library/windows/desktop/dn280510(v=vs.85).aspx
                    return Some(dpi_x as u32)
                }
            }
        }
        None
    }*/
    
    pub fn hwnd_dpi_factor(&self, hwnd: HWND) -> f32 {
        unsafe {
            let hdc = GetDC(Some(hwnd));
            if hdc.is_invalid() {
                panic!("`GetDC` returned null!");
            }
            let dpi = if let Some(get_dpi_for_window) = self.get_dpi_for_window {
                // We are on Windows 10 Anniversary Update (1607) or later.
                match get_dpi_for_window(hwnd) {
                    0 => BASE_DPI, // 0 is returned if hwnd is invalid
                    dpi => dpi as u32,
                }
            }
            else if let Some(get_dpi_for_monitor) = self.get_dpi_for_monitor {
                // We are on Windows 8.1 or later.
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                if monitor.is_invalid() {
                    BASE_DPI
                }
                else {
                    let mut dpi_x = 0;
                    let mut dpi_y = 0;
                    if get_dpi_for_monitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == S_OK {
                        dpi_x as u32
                    } else {
                        BASE_DPI
                    }
                }
            }
            else {
                // We are on Vista or later.
                if IsProcessDPIAware() == TRUE {
                    // If the process is DPI aware, then scaling must be handled by the application using
                    // this DPI value.
                    GetDeviceCaps(Some(hdc), LOGPIXELSX) as u32
                } else {
                    // If the process is DPI unaware, then scaling is performed by the OS; we thus return
                    // 96 (scale factor 1.0) to prevent the window from being re-scaled by both the
                    // application and the WM.
                    BASE_DPI
                }
            };
            dpi as f32 / BASE_DPI as f32
        }
    }
    
}

