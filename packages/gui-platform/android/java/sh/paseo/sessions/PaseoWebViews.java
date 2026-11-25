package sh.paseo.sessions;

import android.content.Context;
import android.os.Build;
import android.util.Log;
import android.util.LongSparseArray;
import android.view.ActionMode;
import android.view.Gravity;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewConfiguration;
import android.view.inputmethod.InputMethodManager;
import android.webkit.JavascriptInterface;
import android.webkit.RenderProcessGoneDetail;
import android.webkit.WebResourceError;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;

import java.util.ArrayDeque;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.FutureTask;

/** Native views overlay the Makepad surface; only the activity thread touches them. */
final class PaseoWebViews {
    private static final String TAG = "PaseoWebViews";
    private final MainActivity activity;
    private final LongSparseArray<FormView> views = new LongSparseArray<>();
    private boolean closed;

    PaseoWebViews(MainActivity activity) {
        this.activity = activity;
    }

    boolean supported() {
        FutureTask<Boolean> probe = new FutureTask<>(() -> {
            if (Build.VERSION.SDK_INT >= 26) {
                if (WebView.getCurrentWebViewPackage() != null) return true;
                Log.w(TAG, "No Android WebView provider is installed");
                return false;
            }
            // The provider query did not exist on API 24–25.
            new WebView(activity).destroy();
            return true;
        });
        activity.runOnUiThread(probe);
        try {
            return probe.get();
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            Log.w(TAG, "Interrupted checking Android WebView", error);
        } catch (ExecutionException error) {
            Log.w(TAG, "Android WebView is unavailable", error.getCause());
        }
        return false;
    }

    void create(long id, String html, String bootstrap, int background) {
        activity.runOnUiThread(() -> {
            if (closed) {
                onFailed(id, "The Android activity was destroyed");
                return;
            }
            try {
                FrameLayout root = activity.findViewById(android.R.id.content);
                FormView form = new FormView(id, root, html, bootstrap, background);
                views.put(id, form);
                root.addView(form.clip, new FrameLayout.LayoutParams(1, 1));
            } catch (RuntimeException error) {
                destroyNow(id);
                Log.w(TAG, "Cannot create Android WebView", error);
                onFailed(id, "Cannot create Android WebView");
            }
        });
    }

    void place(long id, int x, int y, int width, int height,
               int left, int top, int clipWidth, int clipHeight, double dpi) {
        activity.runOnUiThread(() -> {
            FormView form = views.get(id);
            if (form != null) form.place(x, y, width, height, left, top, clipWidth, clipHeight, dpi);
        });
    }

    void evaluate(long id, String script) {
        activity.runOnUiThread(() -> {
            FormView form = views.get(id);
            if (form != null) form.evaluate(script);
        });
    }

    void blur(long id) {
        activity.runOnUiThread(() -> {
            FormView form = views.get(id);
            if (form != null) form.blur();
        });
    }

    void destroy(long id) {
        activity.runOnUiThread(() -> destroyNow(id));
    }

    void close() {
        closed = true;
        while (views.size() != 0) {
            views.valueAt(0).fail("The Android activity was destroyed");
        }
    }

    private void destroyNow(long id) {
        FormView form = views.get(id);
        if (form == null) return;
        views.remove(id);
        form.blur();
        form.root.removeView(form.clip);
        form.clip.removeView(form);
        form.removeJavascriptInterface("PaseoBridge");
        form.destroy();
    }

    private final class FormView extends WebView {
        final long id;
        final FrameLayout root;
        final FrameLayout clip;
        private final int touchSlop;
        private final int[] renderPosition = new int[2];
        private final int[] rootPosition = new int[2];
        private final ArrayDeque<String> pendingScripts = new ArrayDeque<>();
        private String html, bootstrap;
        private boolean started, ready, visible, tracking, dragging;
        private float downX, downY, lastY;
        private double dpi;

        FormView(long id, FrameLayout root, String html, String bootstrap, int background) {
            super(activity);
            this.id = id;
            this.root = root;
            this.html = html;
            this.bootstrap = bootstrap;
            clip = new FrameLayout(activity);
            clip.setClipChildren(true);
            clip.setVisibility(INVISIBLE);
            clip.addView(this, new FrameLayout.LayoutParams(1, 1));
            touchSlop = ViewConfiguration.get(activity).getScaledTouchSlop();

            setBackgroundColor(background);
            WebView.setWebContentsDebuggingEnabled(false);
            setFocusableInTouchMode(true);
            setOnLongClickListener(view -> true);
            setLongClickable(false);
            setHorizontalScrollBarEnabled(false);
            setVerticalScrollBarEnabled(false);
            setOverScrollMode(OVER_SCROLL_NEVER);

            WebSettings settings = getSettings();
            // The document CSP denies script markup and handlers; evaluation injects our bridge.
            settings.setJavaScriptEnabled(true);
            settings.setAllowFileAccess(false);
            settings.setAllowContentAccess(false);
            settings.setSupportMultipleWindows(false);
            settings.setJavaScriptCanOpenWindowsAutomatically(false);
            settings.setSupportZoom(false);
            settings.setTextZoom(100);
            settings.setUseWideViewPort(false);

            addJavascriptInterface(new MessageBridge(id), "PaseoBridge");
            setWebViewClient(new WebViewClient() {
                @Override
                public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
                    return true;
                }

                @Override
                @SuppressWarnings("deprecation")
                public boolean shouldOverrideUrlLoading(WebView view, String url) {
                    return true;
                }

                @Override
                public void onPageStarted(WebView view, String url, android.graphics.Bitmap icon) {
                    // shouldOverrideUrlLoading is not called for our app-initiated data load.
                    if (!started && initialDocument(url)) {
                        started = true;
                    } else {
                        view.stopLoading();
                        fail("Android WebView refused an unexpected document");
                    }
                }

                @Override
                public void onPageFinished(WebView view, String url) {
                    if (views.get(id) != FormView.this || !started || ready || !initialDocument(url)) return;
                    view.evaluateJavascript(bootstrapScript(), null);
                    ready = true;
                    while (!pendingScripts.isEmpty()) view.evaluateJavascript(pendingScripts.remove(), null);
                    updateVisibility();
                }

                @Override
                public void onReceivedError(WebView view, WebResourceRequest request, WebResourceError error) {
                    if (request.isForMainFrame()) fail("Android WebView could not load the form (" + error.getErrorCode() + ")");
                }

                @Override
                public boolean onRenderProcessGone(WebView view, RenderProcessGoneDetail detail) {
                    fail(detail.didCrash() ? "Android WebView renderer crashed" : "Android WebView renderer was stopped");
                    return true;
                }
            });
        }

        private String bootstrapScript() {
            String script = bootstrap;
            bootstrap = null;
            return script;
        }

        private boolean initialDocument(String url) {
            return "about:blank".equals(url) || (url != null && url.startsWith("data:text/html"));
        }

        void place(int x, int y, int width, int height,
                   int left, int top, int clipWidth, int clipHeight, double newDpi) {
            visible = clipWidth > 0 && clipHeight > 0;
            if (!visible) blur();
            updateVisibility();
            if (width <= 0 || height <= 0) return;

            if (visible) {
                activity.getRenderView().getLocationInWindow(renderPosition);
                root.getLocationInWindow(rootPosition);
                setFrame(clip, left + renderPosition[0] - rootPosition[0],
                        top + renderPosition[1] - rootPosition[1], clipWidth, clipHeight);
            }
            setFrame(this, visible ? x - left : 0, visible ? y - top : 0, width, height);

            if (html != null) {
                // WebView normally maps a CSS px to the display density. Makepad can override it.
                if (newDpi != getResources().getDisplayMetrics().density) {
                    setInitialScale((int) Math.round(newDpi * 100));
                }
                dpi = newDpi;
                String document = html;
                html = null;
                loadDataWithBaseURL(null, document, "text/html", "utf-8", null);
            } else if (newDpi != dpi) {
                zoomBy((float) (newDpi / dpi));
                dpi = newDpi;
            }
        }

        private void updateVisibility() {
            clip.setVisibility(visible && ready ? VISIBLE : INVISIBLE);
        }

        void evaluate(String script) {
            if (ready) evaluateJavascript(script, null);
            else pendingScripts.add(script);
        }

        void blur() {
            if (!hasFocus()) return;
            activity.getSystemService(InputMethodManager.class).hideSoftInputFromWindow(getWindowToken(), 0);
            clearFocus();
            activity.getRenderView().requestFocus();
        }

        void fail(String reason) {
            if (views.get(id) != this) return;
            destroyNow(id);
            onFailed(id, reason);
        }

        @Override
        public ActionMode startActionMode(ActionMode.Callback callback) {
            return null;
        }

        @Override
        public ActionMode startActionMode(ActionMode.Callback callback, int type) {
            return null;
        }

        @Override
        public boolean showContextMenu() {
            return false;
        }

        @Override
        public boolean showContextMenu(float x, float y) {
            return false;
        }

        @Override
        public boolean dispatchTouchEvent(MotionEvent event) {
            switch (event.getActionMasked()) {
                case MotionEvent.ACTION_DOWN:
                    tracking = true;
                    dragging = false;
                    downX = event.getRawX();
                    downY = lastY = event.getRawY();
                    requestFocus();
                    break;
                case MotionEvent.ACTION_POINTER_DOWN:
                    tracking = false;
                    break;
                case MotionEvent.ACTION_MOVE:
                    if (tracking) {
                        // Screen coordinates stay put while scrolling repositions this view.
                        float y = event.getRawY();
                        if (!dragging && Math.abs(y - downY) > touchSlop
                                && Math.abs(y - downY) > Math.abs(event.getRawX() - downX)) {
                            MotionEvent cancel = MotionEvent.obtain(event);
                            cancel.setAction(MotionEvent.ACTION_CANCEL);
                            super.dispatchTouchEvent(cancel);
                            cancel.recycle();
                            dragging = true;
                        }
                        if (dragging) {
                            onScroll(id, (lastY - y) / dpi);
                            lastY = y;
                            return true;
                        }
                    }
                    break;
                case MotionEvent.ACTION_UP:
                case MotionEvent.ACTION_CANCEL:
                    tracking = false;
                    if (dragging) {
                        dragging = false;
                        return true;
                    }
                    break;
                default:
                    break;
            }
            return dragging || super.dispatchTouchEvent(event);
        }
    }

    private static void setFrame(View view, int x, int y, int width, int height) {
        FrameLayout.LayoutParams params = (FrameLayout.LayoutParams) view.getLayoutParams();
        if (params.width == width && params.height == height && params.leftMargin == x
                && params.topMargin == y && params.gravity == (Gravity.TOP | Gravity.LEFT)) return;
        params.width = width;
        params.height = height;
        params.leftMargin = x;
        params.topMargin = y;
        params.gravity = Gravity.TOP | Gravity.LEFT;
        view.setLayoutParams(params);
    }

    private static final class MessageBridge {
        private final long id;

        MessageBridge(long id) { this.id = id; }

        @JavascriptInterface
        public void postMessage(String message) { onMessage(id, message); }
    }

    private static native void onMessage(long id, String message);
    private static native void onScroll(long id, double dy);
    private static native void onFailed(long id, String reason);
}
