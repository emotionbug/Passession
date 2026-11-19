package dev.makepad.android;

import android.graphics.Rect;
import android.os.Build;
import android.os.Bundle;
import android.util.LongSparseArray;
import android.util.SparseArray;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewParent;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityManager;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityNodeProvider;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;

/** SurfaceView has no native children; this provider presents Makepad's drawn controls. */
final class MakepadAccessibility extends AccessibilityNodeProvider {
    private static final int NONE = Integer.MIN_VALUE;
    // Role and bit ordinals from makepad-platform/src/os/linux/android/android_jni.rs.
    private static final String[] CLASSES = {"android.widget.Button", "android.widget.EditText",
        "android.widget.TextView", "android.view.View", "android.widget.TextView",
        "android.widget.CheckBox", "android.widget.Switch", "android.widget.SeekBar",
        "android.widget.ScrollView"};
    private static final int TEXT_FIELD = 1, TEXT = 2, HEADER = 4, CHECKBOX = 5, SWITCH = 6, SCROLL = 8;
    private static final int ENABLED = 1, SELECTED = 2, CHECKED = 4, FOCUSED = 8;
    private static final int CLICK = 1, FOCUS = 2, FORWARD = 4, BACKWARD = 8;

    private final MakepadSurface host;
    private final AccessibilityManager manager;
    private final AccessibilityManager.AccessibilityStateChangeListener stateListener = enabled -> refresh();
    private final AccessibilityManager.TouchExplorationStateChangeListener touchListener = enabled -> refresh();
    private final int[] location = new int[2];
    private final Rect visible = new Rect();
    private final ArrayList<Node> ordered = new ArrayList<>();
    private final SparseArray<Node> nodes = new SparseArray<>();
    private LongSparseArray<Integer> virtualIds = new LongSparseArray<>();
    private int nextId = 1, accessibilityFocus = NONE, hover = NONE;
    private boolean observing, active;
    private long revision;
    private int inputFocus = NONE, pendingScroll = NONE;

    private static final class Node {
        final long id, parent;
        final Rect bounds;
        final int virtualId, role, state, actions;
        int previousSibling = NONE;
        final String label;
        int virtualParent = HOST_VIEW_ID;

        Node(ByteBuffer data, int virtualId, long id) {
            this.id = id;
            this.virtualId = virtualId;
            parent = data.getLong();
            bounds = new Rect(data.getInt(), data.getInt(), data.getInt(), data.getInt());
            role = data.get();
            state = data.get();
            actions = data.get();
            int length = data.getInt();
            label = new String(data.array(), data.position(), length, StandardCharsets.UTF_8);
            data.position(data.position() + length);
        }
    }

    MakepadAccessibility(MakepadSurface host) {
        this.host = host;
        manager = host.getContext().getSystemService(AccessibilityManager.class);
    }

    boolean isActive() { return active && MakepadActivity.isLive(host.getContext()); }

    void start() {
        if (observing) return;
        manager.addAccessibilityStateChangeListener(stateListener);
        manager.addTouchExplorationStateChangeListener(touchListener);
        observing = true;
        refresh();
    }

    void stop() {
        if (!observing) return;
        manager.removeAccessibilityStateChangeListener(stateListener);
        manager.removeTouchExplorationStateChangeListener(touchListener);
        observing = false;
        refresh();
    }

    private void refresh() {
        boolean enabled = observing && (manager.isEnabled() || manager.isTouchExplorationEnabled());
        if (enabled == active) return;
        active = enabled;
        if (MakepadActivity.isLive(host.getContext())) MakepadNative.setAccessibilityEnabled(enabled);
        if (!enabled) clear();
    }

    void clear() {
        if (ordered.isEmpty()) return;
        send(hover, AccessibilityEvent.TYPE_VIEW_HOVER_EXIT);
        send(accessibilityFocus, AccessibilityEvent.TYPE_VIEW_ACCESSIBILITY_FOCUS_CLEARED);
        hover = accessibilityFocus = NONE;
        inputFocus = pendingScroll = NONE;
        ordered.clear();
        nodes.clear();
        virtualIds.clear();
        send(HOST_VIEW_ID, AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED);
    }

    void update(byte[] bytes) {
        if (!isActive()) return;
        ByteBuffer data = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
        revision = data.getLong();
        ArrayList<Node> next = new ArrayList<>();
        LongSparseArray<Integer> ids = new LongSparseArray<>();
        while (data.hasRemaining()) {
            long id = data.getLong();
            Integer previous = virtualIds.get(id);
            Node node = new Node(data, previous != null ? previous : nextId++, id);
            ids.put(id, node.virtualId);
            next.add(node);
        }
        Node oldFocus = nodes.get(accessibilityFocus);
        if (oldFocus != null && ids.indexOfKey(oldFocus.id) < 0) {
            send(accessibilityFocus, AccessibilityEvent.TYPE_VIEW_ACCESSIBILITY_FOCUS_CLEARED);
            accessibilityFocus = NONE;
        }
        Node oldHover = nodes.get(hover);
        if (oldHover != null && ids.indexOfKey(oldHover.id) < 0) {
            send(hover, AccessibilityEvent.TYPE_VIEW_HOVER_EXIT);
            hover = NONE;
        }
        virtualIds = ids;
        ordered.clear();
        nodes.clear();
        SparseArray<Integer> previousSiblings = new SparseArray<>();
        int nextFocus = NONE;
        for (Node node : next) {
            node.virtualParent = ids.get(node.parent, HOST_VIEW_ID);
            node.previousSibling = previousSiblings.get(node.virtualParent, NONE);
            previousSiblings.put(node.virtualParent, node.virtualId);
            ordered.add(node);
            nodes.put(node.virtualId, node);
            if ((node.state & FOCUSED) != 0 && (node.actions & FOCUS) != 0) nextFocus = node.virtualId;
        }
        send(HOST_VIEW_ID, AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED);
        if (nextFocus != inputFocus) {
            inputFocus = nextFocus;
            send(inputFocus, AccessibilityEvent.TYPE_VIEW_FOCUSED);
        }
        if (pendingScroll != NONE && nodes.get(pendingScroll) != null) {
            send(pendingScroll, AccessibilityEvent.TYPE_VIEW_SCROLLED);
        }
        pendingScroll = NONE;
    }

    @Override
    public AccessibilityNodeInfo createAccessibilityNodeInfo(int virtualId) {
        if (!isActive()) return null;
        if (virtualId == HOST_VIEW_ID) {
            AccessibilityNodeInfo info = AccessibilityNodeInfo.obtain(host);
            host.onInitializeAccessibilityNodeInfo(info);
            addChildren(info, virtualId);
            return info;
        }
        Node node = nodes.get(virtualId);
        if (node == null) return null;
        AccessibilityNodeInfo info = AccessibilityNodeInfo.obtain();
        info.setSource(host, virtualId);
        if (node.virtualParent == HOST_VIEW_ID) info.setParent(host);
        else info.setParent(host, node.virtualParent);
        info.setPackageName(host.getContext().getPackageName());
        info.setClassName(CLASSES[node.role]);
        if (node.role == TEXT || node.role == HEADER) info.setText(node.label);
        else info.setContentDescription(node.label);
        if (Build.VERSION.SDK_INT >= 28) info.setHeading(node.role == HEADER);
        info.setEnabled((node.state & ENABLED) != 0);
        info.setSelected((node.state & SELECTED) != 0);
        info.setCheckable(node.role == CHECKBOX || node.role == SWITCH);
        info.setChecked((node.state & CHECKED) != 0);
        info.setEditable(node.role == TEXT_FIELD);
        info.setFocusable(true);
        info.setFocused((node.state & FOCUSED) != 0);
        info.setClickable((node.actions & CLICK) != 0);
        info.setScrollable((node.actions & (FORWARD | BACKWARD)) != 0);
        if ((node.actions & CLICK) != 0) info.addAction(AccessibilityNodeInfo.ACTION_CLICK);
        if (node.previousSibling != NONE) info.setTraversalAfter(host, node.previousSibling);
        if ((node.actions & FOCUS) != 0) info.addAction(AccessibilityNodeInfo.ACTION_FOCUS);
        if ((node.actions & FORWARD) != 0) info.addAction(AccessibilityNodeInfo.ACTION_SCROLL_FORWARD);
        if ((node.actions & BACKWARD) != 0) info.addAction(AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD);
        info.setAccessibilityFocused(virtualId == accessibilityFocus);
        info.addAction(virtualId == accessibilityFocus ? AccessibilityNodeInfo.ACTION_CLEAR_ACCESSIBILITY_FOCUS
            : AccessibilityNodeInfo.ACTION_ACCESSIBILITY_FOCUS);
        Rect bounds = new Rect(node.bounds);
        Node parent = nodes.get(node.virtualParent);
        if (parent != null) bounds.offset(-parent.bounds.left, -parent.bounds.top);
        info.setBoundsInParent(bounds);
        bounds.set(node.bounds);
        info.setVisibleToUser(host.isShown() && host.getLocalVisibleRect(visible) && bounds.intersect(visible));
        host.getLocationOnScreen(location);
        bounds.offset(location[0], location[1]);
        info.setBoundsInScreen(bounds);
        addChildren(info, virtualId);
        return info;
    }

    private void addChildren(AccessibilityNodeInfo info, int parent) {
        for (Node node : ordered) {
            if (node.virtualParent != parent) continue;
            info.addChild(host, node.virtualId);
        }
    }

    @Override
    public AccessibilityNodeInfo findFocus(int focus) {
        if (focus == AccessibilityNodeInfo.FOCUS_ACCESSIBILITY) return createAccessibilityNodeInfo(accessibilityFocus);
        if (focus == AccessibilityNodeInfo.FOCUS_INPUT) return createAccessibilityNodeInfo(inputFocus);
        return null;
    }

    @Override
    public boolean performAction(int virtualId, int action, Bundle args) {
        if (!isActive()) return false;
        if (virtualId == HOST_VIEW_ID) return host.performAccessibilityAction(action, args);
        Node node = nodes.get(virtualId);
        if (node == null) return false;
        switch (action) {
            case AccessibilityNodeInfo.ACTION_ACCESSIBILITY_FOCUS:
                if (accessibilityFocus == virtualId) return false;
                int previous = accessibilityFocus;
                accessibilityFocus = virtualId;
                send(previous, AccessibilityEvent.TYPE_VIEW_ACCESSIBILITY_FOCUS_CLEARED);
                send(virtualId, AccessibilityEvent.TYPE_VIEW_ACCESSIBILITY_FOCUSED);
                return true;
            case AccessibilityNodeInfo.ACTION_CLEAR_ACCESSIBILITY_FOCUS:
                if (accessibilityFocus != virtualId) return false;
                accessibilityFocus = NONE;
                send(virtualId, AccessibilityEvent.TYPE_VIEW_ACCESSIBILITY_FOCUS_CLEARED);
                return true;
            default:
                break;
        }
        int supported;
        switch (action) {
            case AccessibilityNodeInfo.ACTION_CLICK: supported = CLICK; break;
            case AccessibilityNodeInfo.ACTION_FOCUS: supported = FOCUS; break;
            case AccessibilityNodeInfo.ACTION_SCROLL_FORWARD: supported = FORWARD; break;
            case AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD: supported = BACKWARD; break;
            default: return false;
        }
        if ((node.state & ENABLED) == 0 || (node.actions & supported) == 0) return false;
        MakepadNative.performAccessibilityAction(node.id, revision, action);
        if (supported == FORWARD || supported == BACKWARD) pendingScroll = virtualId;
        if (supported == CLICK) send(virtualId, AccessibilityEvent.TYPE_VIEW_CLICKED);
        return true;
    }

    boolean hover(MotionEvent event) {
        if (!isActive() || !manager.isTouchExplorationEnabled()) return false;
        int target = NONE;
        switch (event.getActionMasked()) {
            case MotionEvent.ACTION_HOVER_ENTER:
            case MotionEvent.ACTION_HOVER_MOVE:
                int x = (int) event.getX(), y = (int) event.getY();
                for (int i = ordered.size() - 1; i >= 0; i--) {
                    Node node = ordered.get(i);
                    if (node.role != SCROLL && node.bounds.contains(x, y)) {
                        target = node.virtualId;
                        break;
                    }
                }
                break;
            case MotionEvent.ACTION_HOVER_EXIT:
                break;
            default: return false;
        }
        if (target != hover) {
            int previous = hover;
            hover = target;
            send(target, AccessibilityEvent.TYPE_VIEW_HOVER_ENTER);
            send(previous, AccessibilityEvent.TYPE_VIEW_HOVER_EXIT);
        }
        return true;
    }

    private void send(int id, int type) {
        if (id == NONE || !manager.isEnabled()) return;
        ViewParent parent = host.getParent();
        if (parent == null) return;
        AccessibilityEvent event = AccessibilityEvent.obtain(type);
        event.setPackageName(host.getContext().getPackageName());
        Node node = nodes.get(id);
        if (node != null) {
            event.setSource(host, id);
            event.setClassName(CLASSES[node.role]);
            event.setContentDescription(node.label);
            event.setEnabled((node.state & ENABLED) != 0);
        } else {
            event.setSource(host);
            event.setClassName(View.class.getName());
            event.setEnabled(host.isEnabled());
        }
        if (type == AccessibilityEvent.TYPE_WINDOW_CONTENT_CHANGED) {
            event.setContentChangeTypes(AccessibilityEvent.CONTENT_CHANGE_TYPE_SUBTREE);
        }
        parent.requestSendAccessibilityEvent(host, event);
    }
}
