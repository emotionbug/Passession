package dev.makepad.android;

import android.app.Activity;
import android.bluetooth.BluetoothAdapter;
import android.bluetooth.BluetoothDevice;
import android.bluetooth.BluetoothManager;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.graphics.Color;
import android.graphics.Rect;
import android.media.AudioDeviceInfo;
import android.media.AudioManager;
import android.media.midi.MidiDevice;
import android.media.midi.MidiDeviceInfo;
import android.media.midi.MidiManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.os.SystemClock;
import android.util.Log;
import android.view.ActionMode;
import android.view.Display;
import android.view.KeyEvent;
import android.view.Menu;
import android.view.MenuItem;
import android.view.MotionEvent;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewConfiguration;
import android.view.ViewTreeObserver;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.view.WindowManager.LayoutParams;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.LinearLayout;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.Set;
import java.util.concurrent.CompletableFuture;

// note: //% is a special miniquad's pre-processor for plugins
// when there are no plugins - //% whatever will be replaced to an empty string
// before compiling

//% IMPORTS

class MakepadSurface
    extends
        SurfaceView
    implements
        View.OnTouchListener,
        View.OnKeyListener,
        View.OnLongClickListener,
        ViewTreeObserver.OnGlobalLayoutListener,
        SurfaceHolder.Callback
{

    // The X,Y coordinates and pointer ID of the most recent ACTION_DOWN touch.
    private float latestDownTouchX;
    private float latestDownTouchY;
    private int latestDownTouchPointerId;

    // The X,Y coordinates and pointer ID of the most recent non-ACTION_DOWN touch event.
    private float latestTouchX;
    private float latestTouchY;
    private int latestTouchPointerId;

    // Paseo: native widgets share one SurfaceView but not one editing session.
    private long inputSession;
    private MakepadInputConnection inputConnection;
    private boolean inputPassword, inputNumeric;

    private static final int CAN_COPY = 1, CAN_CUT = 2, CAN_PASTE = 4, CAN_SELECT_ALL = 8;
    private static final int ACTION_DISMISS = 0, ACTION_COPY = 1, ACTION_CUT = 2,
        ACTION_PASTE = 3, ACTION_SELECT_ALL = 4;
    private final Handler textActionHandler = new Handler(Looper.getMainLooper());
    private final Object textActionLock = new Object();
    private final ClipboardManager textActionClipboard;
    // Tokens increase across native owners. Retired tokens cannot reopen a menu
    // while a native frame is still catching up with a Java dismissal.
    private long latestTextActionToken, requestedTextActionToken, textActionRequestGeneration;
    private int requestedTextActionCapabilities;
    private boolean textActionsResumed, textActionsSurfaceReady, textActionBackPressed;
    private TextActions textActions;

    void requestTextActions(long token, int flags, int left, int top, int right, int bottom) {
        synchronized (textActionLock) {
            if (token <= 0 || token < latestTextActionToken
                    || (token == latestTextActionToken && requestedTextActionToken == 0)) return;
            latestTextActionToken = requestedTextActionToken = token;
            requestedTextActionCapabilities = flags & (CAN_COPY | CAN_CUT | CAN_PASTE | CAN_SELECT_ALL);
            long generation = ++textActionRequestGeneration;
            textActionHandler.post(() -> {
                synchronized (textActionLock) {
                    if (generation != textActionRequestGeneration) return;
                }
                showTextActions(token, flags, left, top, right, bottom);
            });
        }
    }

    void requestHideTextActions() {
        synchronized (textActionLock) {
            requestedTextActionToken = 0;
            long generation = ++textActionRequestGeneration;
            textActionHandler.post(() -> {
                synchronized (textActionLock) {
                    if (generation != textActionRequestGeneration) return;
                }
                finishTextActions();
            });
        }
    }

    private void showTextActions(long token, int flags, int left, int top, int right, int bottom) {
        boolean available = textActionsResumed && textActionsSurfaceReady
            && isAttachedToWindow() && hasWindowFocus() && hasFocus();
        if (!available) {
            dismissTextActions(token);
            return;
        }
        flags &= CAN_COPY | CAN_CUT | CAN_PASTE | CAN_SELECT_ALL;
        if (textActions != null && textActions.token == token) {
            TextActions current = textActions;
            if (current.capabilities != flags) {
                current.capabilities = flags;
                current.mode.invalidate();
            }
            Rect rect = current.contentRect;
            if (rect.left != left || rect.top != top || rect.right != right || rect.bottom != bottom) {
                rect.set(left, top, right, bottom);
                current.mode.invalidateContentRect();
            }
            return;
        }
        finishTextActions();
        TextActions next = new TextActions(token, flags, left, top, right, bottom);
        textActions = next;
        ActionMode mode = startActionMode(next, ActionMode.TYPE_FLOATING);
        if (mode == null) {
            dismissTextActions(token);
        } else if (!next.isCurrent()) {
            finishTextActions();
        } else {
            next.mode = mode;
            textActionClipboard.addPrimaryClipChangedListener(next);
            next.observingClipboard = true;
        }
    }

    private void finishTextActions() {
        TextActions current = textActions;
        textActions = null;
        if (current == null) return;
        // finish() may synchronously invoke onDestroyActionMode. Never let a
        // replacement/hide callback dismiss the next native selection owner.
        current.finished = true;
        current.stopObservingClipboard();
        if (current.mode != null) current.mode.finish();
    }

    private void dismissTextActions(long token) {
        boolean notifyNative;
        synchronized (textActionLock) {
            notifyNative = requestedTextActionToken == token;
            if (notifyNative) {
                requestedTextActionToken = 0;
                ++textActionRequestGeneration;
            }
        }
        if (textActions != null && textActions.token == token) finishTextActions();
        if (notifyNative) {
            if (inputConnection != null) inputConnection.flushBeforeNativeAction();
            MakepadNative.surfaceOnTextAction(token, ACTION_DISMISS, null);
        }
    }

    void cancelTextActions() {
        long token;
        synchronized (textActionLock) {
            token = requestedTextActionToken;
            requestedTextActionToken = 0;
            ++textActionRequestGeneration;
        }
        finishTextActions();
        if (token != 0) {
            if (inputConnection != null) inputConnection.flushBeforeNativeAction();
            MakepadNative.surfaceOnTextAction(token, ACTION_DISMISS, null);
        }
    }

    boolean dismissTextActionsOnBack() {
        synchronized (textActionLock) {
            if (requestedTextActionToken == 0 && textActions == null) return false;
        }
        cancelTextActions();
        return true;
    }

    void setTextActionsResumed(boolean resumed) {
        if (textActionsResumed == resumed) return;
        textActionsResumed = resumed;
        cancelTextActions();
    }

    @Override
    public void onWindowFocusChanged(boolean hasWindowFocus) {
        super.onWindowFocusChanged(hasWindowFocus);
        cancelTextActions();
    }

    @Override
    protected void onFocusChanged(boolean gainFocus, int direction, Rect previouslyFocusedRect) {
        super.onFocusChanged(gainFocus, direction, previouslyFocusedRect);
        cancelTextActions();
    }

    @Override
    protected void onDetachedFromWindow() {
        cancelTextActions();
        super.onDetachedFromWindow();
    }

    private final class TextActions extends ActionMode.Callback2
            implements ClipboardManager.OnPrimaryClipChangedListener {
        final long token;
        final Rect contentRect;
        int capabilities, menuFlags = -1;
        ActionMode mode;
        boolean finished, observingClipboard;

        TextActions(long token, int flags, int left, int top, int right, int bottom) {
            this.token = token;
            capabilities = flags;
            contentRect = new Rect(left, top, right, bottom);
        }

        boolean isCurrent() {
            if (finished || textActions != this) return false;
            synchronized (textActionLock) {
                return requestedTextActionToken == token;
            }
        }

        boolean updateMenu(Menu menu) {
            int flags = capabilities;
            if (!textActionClipboard.hasPrimaryClip()) flags &= ~CAN_PASTE;
            if (flags == menuFlags) return false;
            menuFlags = flags;
            menu.clear();
            if ((flags & CAN_CUT) != 0) menu.add(Menu.NONE, android.R.id.cut, 0,
                android.R.string.cut).setShowAsAction(MenuItem.SHOW_AS_ACTION_IF_ROOM);
            if ((flags & CAN_COPY) != 0) menu.add(Menu.NONE, android.R.id.copy, 1,
                android.R.string.copy).setShowAsAction(MenuItem.SHOW_AS_ACTION_IF_ROOM);
            if ((flags & CAN_PASTE) != 0) menu.add(Menu.NONE, android.R.id.paste, 2,
                android.R.string.paste).setShowAsAction(MenuItem.SHOW_AS_ACTION_IF_ROOM);
            if ((flags & CAN_SELECT_ALL) != 0) menu.add(Menu.NONE, android.R.id.selectAll, 3,
                android.R.string.selectAll).setShowAsAction(MenuItem.SHOW_AS_ACTION_IF_ROOM);
            return true;
        }

        @Override
        public boolean onCreateActionMode(ActionMode mode, Menu menu) {
            this.mode = mode;
            if (!isCurrent()) return false;
            updateMenu(menu);
            return menu.size() > 0;
        }

        @Override
        public boolean onPrepareActionMode(ActionMode mode, Menu menu) {
            return isCurrent() && updateMenu(menu);
        }

        @Override
        public void onGetContentRect(ActionMode mode, View view, Rect outRect) {
            // Callback2 expects SurfaceView-local physical pixels.
            outRect.set(contentRect);
        }

        @Override
        public boolean onActionItemClicked(ActionMode mode, MenuItem item) {
            if (!isCurrent()) return false;
            int action, capability;
            switch (item.getItemId()) {
                case android.R.id.copy: action = ACTION_COPY; capability = CAN_COPY; break;
                case android.R.id.cut: action = ACTION_CUT; capability = CAN_CUT; break;
                case android.R.id.paste: action = ACTION_PASTE; capability = CAN_PASTE; break;
                case android.R.id.selectAll:
                    action = ACTION_SELECT_ALL; capability = CAN_SELECT_ALL; break;
                default: return false;
            }
            if ((capabilities & capability) == 0) return false;
            synchronized (textActionLock) {
                if ((requestedTextActionCapabilities & capability) == 0) return false;
            }
            String text = null;
            if (action == ACTION_PASTE) {
                ClipData clip = textActionClipboard.getPrimaryClip();
                if (clip == null || clip.getItemCount() == 0) {
                    mode.invalidate();
                    return false;
                }
                CharSequence value = clip.getItemAt(0).coerceToText(getContext());
                if (value == null) return false;
                text = value.toString();
            }
            if (inputConnection != null) inputConnection.flushBeforeNativeAction();
            synchronized (textActionLock) {
                if (requestedTextActionToken != token
                        || (requestedTextActionCapabilities & capability) == 0) return false;
                if (action != ACTION_SELECT_ALL) {
                    requestedTextActionToken = 0;
                    ++textActionRequestGeneration;
                }
            }
            if (action != ACTION_SELECT_ALL) finishTextActions();
            // Rust edits once and publishes the new IME snapshot. In particular,
            // do not also run InputConnection.performContextMenuAction here.
            MakepadNative.surfaceOnTextAction(token, action, text);
            return true;
        }

        @Override
        public void onDestroyActionMode(ActionMode mode) {
            boolean notifyNative = !finished && textActions == this;
            finished = true;
            stopObservingClipboard();
            if (textActions == this) textActions = null;
            if (notifyNative) dismissTextActions(token);
        }

        @Override
        public void onPrimaryClipChanged() {
            if (isCurrent()) mode.invalidate();
        }

        void stopObservingClipboard() {
            if (!observingClipboard) return;
            observingClipboard = false;
            textActionClipboard.removePrimaryClipChangedListener(this);
        }
    }

    void updateTextInput(long session, long revision, String text, int start, int end,
                         boolean password, boolean numeric, boolean restart) {
        if (session != inputSession) {
            inputSession = session;
            inputPassword = password;
            inputNumeric = numeric;
            if (text == null) {
                if (inputConnection != null) inputConnection.invalidate();
                inputConnection = null;
                // Do not restart IMM with TYPE_NULL between native fields.
            } else if (restart || inputConnection == null) {
                if (inputConnection != null) inputConnection.invalidate();
                inputConnection = new MakepadInputConnection(this, session, text, start, end);
                ((InputMethodManager) getContext().getSystemService(Context.INPUT_METHOD_SERVICE))
                    .restartInput(this);
            } else {
                inputConnection.synchronize(session, text, start, end);
            }
        } else if (inputConnection != null) {
            inputConnection.acknowledge(revision, text, start, end);
        }
    }


    public MakepadSurface(Context context){
        super(context);
        textActionClipboard = (ClipboardManager) context.getSystemService(Context.CLIPBOARD_SERVICE);
        getHolder().addCallback(this);

        setFocusable(true);
        setFocusableInTouchMode(true);
        requestFocus();
        setOnTouchListener(this);
        setOnKeyListener(this);
        setOnLongClickListener(this);        

        getViewTreeObserver().addOnGlobalLayoutListener(this);
    }

    @Override
    public void surfaceCreated(SurfaceHolder holder) {
        Log.i("SAPP", "surfaceCreated");
        textActionsSurfaceReady = true;
        cancelTextActions();
        Surface surface = holder.getSurface();
        //surface.setFrameRate(120f,0);
        MakepadNative.surfaceOnSurfaceCreated(surface);
    }

    @Override
    public void surfaceDestroyed(SurfaceHolder holder) {
        Log.i("SAPP", "surfaceDestroyed");
        textActionsSurfaceReady = false;
        cancelTextActions();
        Surface surface = holder.getSurface();
        MakepadNative.surfaceOnSurfaceDestroyed(surface);
    }

    @Override
    public void surfaceChanged(SurfaceHolder holder,
                               int format,
                               int width,
                               int height) {
        Log.i("SAPP", "surfaceChanged");
        Surface surface = holder.getSurface();
        //surface.setFrameRate(120f,0);
        MakepadNative.surfaceOnSurfaceChanged(surface, width, height);

    }

    @Override
    public boolean onTouch(View view, MotionEvent event) {
        // By default, we return false so that `onLongClick` will trigger.
        boolean retval = false;

        int actionMasked = event.getActionMasked();
        int index = event.getActionIndex();
        int pointerId = event.getPointerId(index);

        // Save the details of the latest touch-down event,
        // such that we can use them in the `onLongClick` method.
        if (actionMasked == MotionEvent.ACTION_DOWN) {
            // Paseo: send accepted batch text before the touch can switch fields.
            if (inputConnection != null) inputConnection.flushBeforeNativeAction();
            latestDownTouchX = event.getX(index);
            latestDownTouchY = event.getY(index);
            // A stationary hold has no MOVE; never compare it with the previous gesture.
            latestTouchX = latestDownTouchX;
            latestTouchY = latestDownTouchY;
            latestDownTouchPointerId = pointerId;
            latestTouchPointerId = -1; // no MOVE has arrived for this gesture.
        }
        else if (actionMasked == MotionEvent.ACTION_MOVE) {
            latestTouchX = event.getX(index);
            latestTouchY = event.getY(index);
            latestTouchPointerId = pointerId;
            if (pointerId == latestDownTouchPointerId) {
                if (isTouchBeyondSlopDistance(view)) {
                    view.cancelLongPress();
                    retval = true;
                }
            }
        }

        MakepadNative.surfaceOnTouch(event);
        return retval;
    }

    @Override
    public boolean onLongClick(View view) {
        long timeMillis = SystemClock.uptimeMillis();

        // If the touch has moved more than the touch slop, ignore this long click.
        if (isTouchBeyondSlopDistance(view)) {
            // Returning false here indicates that we have not handled the long click event,
            // which does *not* trigger the haptic feedback (vibration motor) to buzz.
            return false;
        }

        // Here: a valid long click did occur, and we sholud send that event to makepad.

        // Use the latest touch coordinates if they're the same pointer ID as the initial down touch.
        if (latestTouchPointerId == latestDownTouchPointerId) {
            MakepadNative.surfaceOnLongClick(latestTouchX, latestTouchY, latestDownTouchPointerId, timeMillis);
        }
        // Otherwise, use the coordinates from the original down touch.
        else {
            MakepadNative.surfaceOnLongClick(latestDownTouchX, latestDownTouchY, latestDownTouchPointerId, timeMillis);
        }

        // Returning true here indicates that we have handled the long click event,
        // which triggers the haptic feedback (vibration motor) to buzz.
        return true;
    }

    // Returns true if the distance from the latest touch event to the prior down-touch event
    // is greated than the touch slop distance.
    //
    // If true, this indicates that the touch event shouldn't be considered a press/tap,
    // and is likely a drag or swipe.
    private boolean isTouchBeyondSlopDistance(View view) {
        int touchSlop = ViewConfiguration.get(view.getContext()).getScaledTouchSlop();
        float deltaX = latestTouchX - latestDownTouchX;
        float deltaY = latestTouchY - latestDownTouchY;
        double dist = Math.sqrt((deltaX * deltaX) + (deltaY * deltaY));
        return dist > touchSlop;
    }

    @Override
    public void onGlobalLayout() {
        WindowInsets insets = this.getRootWindowInsets();
        if (insets == null) {
            return;
        }

        Rect r = new Rect();
        this.getWindowVisibleDisplayFrame(r);
        int screenHeight = this.getRootView().getHeight();
        int visibleHeight = r.height();
        int keyboardHeight = screenHeight - visibleHeight;

        // Paseo: WindowInsets.Type (API 30) does not exist on the supported
        // API 24 floor; there, a visible-frame shrink beyond 15% of the root
        // height is the IME.
        boolean keyboardVisible = Build.VERSION.SDK_INT >= 30
            ? insets.isVisible(WindowInsets.Type.ime())
            : keyboardHeight > screenHeight * 0.15f;
        MakepadNative.surfaceOnResizeTextIME(keyboardHeight, keyboardVisible);
    }

    // docs says getCharacters are deprecated
    // but somehow on non-latyn input all keyCode and all the relevant fields in the KeyEvent are zeros
    // and only getCharacters has some usefull data
    @SuppressWarnings("deprecation")
    @Override
    public boolean onKey(View v, int keyCode, KeyEvent event) {
        if (keyCode == KeyEvent.KEYCODE_BACK) {
            if (event.getAction() == KeyEvent.ACTION_DOWN && event.getRepeatCount() == 0) {
                textActionBackPressed = dismissTextActionsOnBack();
            }
            if (textActionBackPressed) {
                if (event.getAction() == KeyEvent.ACTION_UP) textActionBackPressed = false;
                return true;
            }
        }
        if (inputConnection != null && inputConnection.handleTextKey(event)) return true;
        // Shift/Alt change the next composing character; they do not leave the editor.
        if (inputConnection != null && event.getAction() == KeyEvent.ACTION_DOWN
                && !KeyEvent.isModifierKey(keyCode)) {
            inputConnection.flushBeforeNativeAction();
        }
        if (event.getAction() == KeyEvent.ACTION_DOWN && keyCode != 0) {
            int metaState = event.getMetaState();
            MakepadNative.surfaceOnKeyDown(keyCode, metaState);
        }

        if (event.getAction() == KeyEvent.ACTION_UP && keyCode != 0) {
            int metaState = event.getMetaState();
            MakepadNative.surfaceOnKeyUp(keyCode, metaState);
        }

        if (!event.isCtrlPressed() && !event.isAltPressed() && !event.isMetaPressed()
                && (event.getAction() == KeyEvent.ACTION_UP || event.getAction() == KeyEvent.ACTION_MULTIPLE)) {
            String characters = event.getCharacters();
            if (characters != null && !characters.isEmpty()) {
                characters.codePoints().filter(c -> c >= 32 && c != 127)
                    .forEach(MakepadNative::surfaceOnCharacter);
            } else {
                int character = event.getUnicodeChar();
                if (character >= 32 && character != 127) MakepadNative.surfaceOnCharacter(character);
            }
        }

        // if ((keyCode == KeyEvent.KEYCODE_BACK)) {
        //     Log.d("Makepad", "KEYCODE_BACK");
        // }

        if ((keyCode == KeyEvent.KEYCODE_VOLUME_UP) || (keyCode == KeyEvent.KEYCODE_VOLUME_DOWN)) {
            return super.onKeyUp(keyCode, event);
        }

        return true;
    }

    @Override
    public boolean onCheckIsTextEditor() { return inputConnection != null; }

    @Override
    public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
        if (inputConnection != null) inputConnection = inputConnection.reconnect();
        if (inputConnection != null) inputConnection.editorInfo(outAttrs, inputPassword, inputNumeric);
        return inputConnection;
    }

    public Surface getNativeSurface() {
        return getHolder().getSurface();
    }
}

class ResizingLayout
    extends
        LinearLayout
    implements
        View.OnApplyWindowInsetsListener {

    public ResizingLayout(Context context){
        super(context);
        // When viewing in landscape mode with keyboard shown, there are
        // gaps on both sides so we fill the negative space with black.
        setBackgroundColor(Color.BLACK);
        setOnApplyWindowInsetsListener(this);
    }

    @Override
    @SuppressWarnings("deprecation")
    public WindowInsets onApplyWindowInsets(View v, WindowInsets insets) {
        // Paseo: getInsets(Type) is API 30. Before it, the IME is the part of
        // the system window inset that exceeds the stable (navigation) inset.
        int imeBottom = Build.VERSION.SDK_INT >= 30
            ? insets.getInsets(WindowInsets.Type.ime()).bottom
            : Math.max(0, insets.getSystemWindowInsetBottom() - insets.getStableInsetBottom());
        v.setPadding(0, 0, 0, imeBottom);
        return insets;
    }
}

public class MakepadActivity
    extends Activity
    implements MidiManager.OnDeviceOpenedListener
{
    //% MAIN_ACTIVITY_BODY

    private MakepadSurface view;
    Handler mHandler;

    // Paseo: platform insets are relative to the actual resized render surface.
    public View getRenderView() { return view; }

    // video playback
    Handler mVideoPlaybackHandler;
    HashMap<Long, VideoPlayerRunnable> mVideoPlayerRunnables;

    // networking, make these static because of activity switching
    static Handler mWebSocketsHandler;
    static HashMap<Long, MakepadWebSocket> mActiveWebsockets = new HashMap<>();
    static HashMap<Long, MakepadWebSocketReader> mActiveWebsocketsReaders = new HashMap<>();

    static {
        System.loadLibrary("makepad");
    }

    @Override
    public void onCreate(Bundle savedInstanceState) {
        
        HandlerThread webSocketsThreadHandler = new HandlerThread("WebSocketsThread");
        webSocketsThreadHandler.start();
        mWebSocketsHandler = new Handler(webSocketsThreadHandler.getLooper());
        
        super.onCreate(savedInstanceState);
        
        this.requestWindowFeature(Window.FEATURE_NO_TITLE);

        view = new MakepadSurface(this);
        // Put it inside a parent layout which can resize it using padding
        ResizingLayout layout = new ResizingLayout(this);
        layout.addView(view);
        setContentView(layout);

        MakepadNative.activityOnCreate(this);

        HandlerThread decoderThreadHandler = new HandlerThread("VideoPlayerThread");
        decoderThreadHandler.start(); // TODO: only start this if its needed.
        mVideoPlaybackHandler = new Handler(decoderThreadHandler.getLooper());
        mVideoPlayerRunnables = new HashMap<Long, VideoPlayerRunnable>();



        String cache_path = this.getCacheDir().getAbsolutePath();
        float density = getResources().getDisplayMetrics().density;
        boolean isEmulator = this.isEmulator();
        String androidVersion = Build.VERSION.RELEASE;
        String buildNumber = Build.DISPLAY;
        String kernelVersion = this.getKernelVersion();
        int sdkVersion = Build.VERSION.SDK_INT;

        MakepadNative.onAndroidParams(cache_path, density, isEmulator, androidVersion, buildNumber, kernelVersion);

        // Set volume keys to control music stream, we might want make this flexible for app devs
        setVolumeControlStream(AudioManager.STREAM_MUSIC);

        float refreshRate = getDeviceRefreshRate();
        MakepadNative.initChoreographer(refreshRate, sdkVersion);
        //% MAIN_ACTIVITY_ON_CREATE
        
    }

    @Override
    protected void onStart() {
        super.onStart();

       // this forces a high framerate default 
           /*
        Window w = getWindow();
        WindowManager.LayoutParams p = w.getAttributes();
        Display.Mode[] modes = getDisplay().getSupportedModes();

        for(Display.Mode mode: modes){    
            if(mode.getRefreshRate() > 100.0){
                p.preferredDisplayModeId = mode.getModeId();
                w.setAttributes(p);
                Log.w("Makepad", "width"+mode.getRefreshRate()+" id "+mode.getModeId());
                break;
            }
        }
*/      
        MakepadNative.activityOnStart();
    }

    @Override
    protected void onResume() {
        super.onResume();
        view.setTextActionsResumed(true);
        MakepadNative.activityOnResume();

        //% MAIN_ACTIVITY_ON_RESUME
    }
    @Override
    protected void onPause() {
        view.setTextActionsResumed(false);
        super.onPause();
        MakepadNative.activityOnPause();

        //% MAIN_ACTIVITY_ON_PAUSE
    }

    @Override
    protected void onStop() {
        super.onStop();
        MakepadNative.activityOnStop();
    }

    @Override
    protected void onDestroy() {
        view.cancelTextActions();
        super.onDestroy();
        MakepadNative.activityOnDestroy();
    }

    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        if (dismissTextActionsOnBack()) return;
        super.onBackPressed();
        MakepadNative.onBackPressed();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        MakepadNative.activityOnWindowFocusChanged(hasFocus);
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        //% MAIN_ACTIVITY_ON_ACTIVITY_RESULT
    }

    @SuppressWarnings("deprecation")
    public void setFullScreen(final boolean fullscreen) {
        runOnUiThread(new Runnable() {
                @Override
                public void run() {
                    View decorView = getWindow().getDecorView();

                    if (fullscreen) {
                        getWindow().setFlags(LayoutParams.FLAG_LAYOUT_NO_LIMITS, LayoutParams.FLAG_LAYOUT_NO_LIMITS);
                        // Paseo: display cutout modes exist from API 28.
                        if (Build.VERSION.SDK_INT >= 28) {
                            getWindow().getAttributes().layoutInDisplayCutoutMode = LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
                        }
                        if (Build.VERSION.SDK_INT >= 30) {
                            getWindow().setDecorFitsSystemWindows(false);
                        } else {
                            int uiOptions = View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_FULLSCREEN | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY;
                            decorView.setSystemUiVisibility(uiOptions);
                        }
                    }
                    else {
                        if (Build.VERSION.SDK_INT >= 30) {
                            getWindow().setDecorFitsSystemWindows(true);
                        } else {
                          decorView.setSystemUiVisibility(0);
                        }

                    }
                }
            });
    }
    
    public void switchActivityClass(Class c){
        Intent intent = new Intent(getApplicationContext(), c);
        startActivity(intent);
        finish();
    }
    
    // Paseo: JNI runs on Makepad's render thread; Editable and IMM stay on UI.
    public void updateTextInput(long session, long revision, String text, int start, int end,
                                boolean password, boolean numeric, boolean restart) {
        runOnUiThread(() -> view.updateTextInput(session, revision, text, start, end, password, numeric, restart));
    }

    // Paseo: these calls originate on the render thread. The Surface fences
    // requests before posting so queued hides/shows cannot retarget callbacks.
    public void showTextActions(long token, int flags, int left, int top, int right, int bottom) {
        view.requestTextActions(token, flags, left, top, right, bottom);
    }

    public void hideTextActions() {
        view.requestHideTextActions();
    }

    // Called on UI by both legacy and predictive-back application routing.
    public boolean dismissTextActionsOnBack() {
        return view != null && view.dismissTextActionsOnBack();
    }

    public void showKeyboard(final boolean show) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (show) {
                    InputMethodManager imm = (InputMethodManager)getSystemService(Context.INPUT_METHOD_SERVICE);
                    imm.showSoftInput(view, 0);
                } else {
                    InputMethodManager imm = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
                    imm.hideSoftInputFromWindow(view.getWindowToken(),0);
                }
            }
        });
    }

    public void copyToClipboard(String content) {
        ClipboardManager clipboard = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        // User-facing description of the clipboard content
        String clipLabel = getApplicationName() + " clip";
        ClipData clip = ClipData.newPlainText(clipLabel, content);
        clipboard.setPrimaryClip(clip);
    }

    private String getApplicationName() {
        ApplicationInfo applicationInfo = getApplicationContext().getApplicationInfo();
        CharSequence appName = applicationInfo.loadLabel(getPackageManager());
        return appName.toString();
    }

    public void requestHttp(long id, long metadataId, String url, String method, String headers, byte[] body) {
        try {
            MakepadNetwork network = new MakepadNetwork();

            CompletableFuture<HttpResponse> future = network.performHttpRequest(url, method, headers, body);

            future.thenAccept(response -> {
                runOnUiThread(() -> MakepadNative.onHttpResponse(id, metadataId, response.getStatusCode(), response.getHeaders(), response.getBody()));
            }).exceptionally(ex -> {
                runOnUiThread(() -> MakepadNative.onHttpRequestError(id, metadataId, ex.toString()));
                return null;
            });
        } catch (Exception e) {
            MakepadNative.onHttpRequestError(id, metadataId, e.toString());
        }
    }

    public void openWebSocket(long id, String url, long callback) {
        
        MakepadWebSocket webSocket = new MakepadWebSocket(id, url, callback);
        mActiveWebsockets.put(id, webSocket);
        webSocket.connect();

        if (webSocket.isConnected()) {
            MakepadWebSocketReader reader = new MakepadWebSocketReader(this, webSocket);
            mWebSocketsHandler.post(reader);
            mActiveWebsocketsReaders.put(id, reader);
        }
    }

    public void sendWebSocketMessage(long id, byte[] message) {
      
        MakepadWebSocket webSocket = mActiveWebsockets.get(id);
        if (webSocket != null) {
            webSocket.sendMessage(message);
        }
    }

    public void closeWebSocket(long id) {
        
        MakepadWebSocket socket = mActiveWebsockets.get(id);
        if (socket != null) {
            socket.closeSocketAndClearCallback();
        }
        MakepadWebSocketReader reader = mActiveWebsocketsReaders.get(id);
        if (reader != null) {
            mWebSocketsHandler.removeCallbacks(reader);
        }
        
        mActiveWebsocketsReaders.remove(id);
        mActiveWebsockets.remove(id);
    }

    public void webSocketConnectionDone(long id, long callback) {
        mActiveWebsockets.remove(id);
        MakepadNative.onWebSocketClosed(callback);
    }

    public String[] getAudioDevices(long flag){
        try{

            AudioManager am = (AudioManager)this.getSystemService(Context.AUDIO_SERVICE);
            AudioDeviceInfo[] devices = null;
            ArrayList<String> out = new ArrayList<String>();
            if(flag == 0){
                devices = am.getDevices(AudioManager.GET_DEVICES_INPUTS);
            }
            else{
                devices = am.getDevices(AudioManager.GET_DEVICES_OUTPUTS);
            }
            for(AudioDeviceInfo device: devices){
                int[] channel_counts = device.getChannelCounts();
                for(int cc: channel_counts){
                    out.add(String.format(
                        "%d$$%d$$%d$$%s",
                        device.getId(),
                        device.getType(),
                        cc,
                        device.getProductName().toString()
                    ));
                }
            }
            return out.toArray(new String[0]);
        }
        catch(Exception e){
            Log.e("Makepad", "exception: " + e.getMessage());
            Log.e("Makepad", "exception: " + e.toString());
            return null;
        }
    }

    @SuppressWarnings("deprecation")
    public void openAllMidiDevices(long delay){
        Runnable runnable = () -> {
            try{
                BluetoothManager bm = (BluetoothManager) this.getSystemService(Context.BLUETOOTH_SERVICE);
                BluetoothAdapter ba = bm.getAdapter();
                Set<BluetoothDevice> bluetooth_devices = ba.getBondedDevices();
                ArrayList<String> bt_names = new ArrayList<String>();
                MidiManager mm = (MidiManager)this.getSystemService(Context.MIDI_SERVICE);
                for(BluetoothDevice device: bluetooth_devices){
                    if(device.getType() == BluetoothDevice.DEVICE_TYPE_LE){
                        String name =device.getName();
                        bt_names.add(name);
                        mm.openBluetoothDevice(device, this, new Handler(Looper.getMainLooper()));
                    }
                }
                // this appears to give you nonworking BLE midi devices. So we skip those by name (not perfect but ok)
                for (MidiDeviceInfo info : mm.getDevices()){
                    String name = info.getProperties().getCharSequence(MidiDeviceInfo.PROPERTY_NAME).toString();
                    boolean found = false;
                    for (String bt_name : bt_names){
                        if (bt_name.equals(name)){
                            found = true;
                            break;
                        }
                    }
                    if(!found){
                        mm.openDevice(info, this, new Handler(Looper.getMainLooper()));
                    }
                }
            }
            catch(Exception e){
                Log.e("Makepad", "exception: " + e.getMessage());
                Log.e("Makepad", "exception: " + e.toString());
            }
        };
        if(delay != 0){
            mHandler.postDelayed(runnable, delay);
        }
        else{ // run now
            runnable.run();
        }
    }

    public void onDeviceOpened(MidiDevice device) {
        if(device == null){
            return;
        }
        MidiDeviceInfo info = device.getInfo();
        if(info != null){
            String name = info.getProperties().getCharSequence(MidiDeviceInfo.PROPERTY_NAME).toString();
            MakepadNative.onMidiDeviceOpened(name, device);
        }
    }

    public void prepareVideoPlayback(long videoId, Object source, int externalTextureHandle, boolean autoplay, boolean shouldLoop) {
        VideoPlayer VideoPlayer = new VideoPlayer(this, videoId);
        VideoPlayer.setSource(source);
        VideoPlayer.setExternalTextureHandle(externalTextureHandle);
        VideoPlayer.setAutoplay(autoplay);
        VideoPlayer.setShouldLoop(shouldLoop);
        VideoPlayerRunnable runnable = new VideoPlayerRunnable(VideoPlayer);

        mVideoPlayerRunnables.put(videoId, runnable);
        mVideoPlaybackHandler.post(runnable);
    }

    public void beginVideoPlayback(long videoId) {
        VideoPlayerRunnable runnable = mVideoPlayerRunnables.get(videoId);
        if(runnable != null) {
            runnable.beginPlayback();
        }
    }

    public void pauseVideoPlayback(long videoId) {
        VideoPlayerRunnable runnable = mVideoPlayerRunnables.get(videoId);
        if(runnable != null) {
            runnable.pausePlayback();
        }
    }

    public void resumeVideoPlayback(long videoId) {
        VideoPlayerRunnable runnable = mVideoPlayerRunnables.get(videoId);
        if(runnable != null) {
            runnable.resumePlayback();
        }
    }

    public void muteVideoPlayback(long videoId) {
        VideoPlayerRunnable runnable = mVideoPlayerRunnables.get(videoId);
        if(runnable != null) {
            runnable.mute();
        }
    }

    public void unmuteVideoPlayback(long videoId) {
        VideoPlayerRunnable runnable = mVideoPlayerRunnables.get(videoId);
        if(runnable != null) {
            runnable.unmute();
        }
    }

    public void cleanupVideoPlaybackResources(long videoId) {
        VideoPlayerRunnable runnable = mVideoPlayerRunnables.remove(videoId);
        if(runnable != null) {
            runnable.cleanupVideoPlaybackResources();
            runnable = null;
        }
    }
    
                
    public boolean isEmulator() {
        // hints that the app is running on emulator
        return Build.MODEL.startsWith("sdk")
            || "google_sdk".equals(Build.MODEL)
            || Build.MODEL.contains("Emulator")
            || Build.MODEL.contains("Android SDK")
            || Build.MODEL.toLowerCase().contains("droid4x")
            || Build.FINGERPRINT.startsWith("generic")
            || Build.PRODUCT == "sdk"
            || Build.PRODUCT == "google_sdk"
            || (Build.BRAND.startsWith("generic") && Build.DEVICE.startsWith("generic"));
    }

    private String getKernelVersion() {
        try {
            Process process = Runtime.getRuntime().exec("uname -r");
            BufferedReader reader = new BufferedReader(new InputStreamReader(process.getInputStream()));
            StringBuilder stringBuilder = new StringBuilder();
            String line;
            while ((line = reader.readLine()) != null) {
                stringBuilder.append(line);
            }
            return stringBuilder.toString();
        } catch (IOException e) {
            return "Unknown";
        }
    }
    
    

    @SuppressWarnings("deprecation")
    public float getDeviceRefreshRate() {
        float refreshRate = 60.0f;  // Default to a common refresh rate

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // Use getDisplay() API on Android 11 and above
            Display display = getDisplay();
            if (display != null) {
                refreshRate = display.getRefreshRate();
            }
        } else {
            // Use the old method for Android 10 and below
            WindowManager windowManager = (WindowManager) getSystemService(Context.WINDOW_SERVICE);
            if (windowManager != null) {
                Display display = windowManager.getDefaultDisplay();
                refreshRate = display.getRefreshRate();
            }
        }

        return refreshRate;
    }
}
