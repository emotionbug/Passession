package sh.paseo.sessions;

import android.Manifest;
import android.app.Activity;
import android.app.ActivityManager;
import android.app.NotificationManager;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.ClipDescription;
import android.content.ClipboardManager;
import android.content.ContentResolver;
import android.content.ContentValues;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.database.Cursor;
import android.graphics.Color;
import android.graphics.Insets;
import android.graphics.Rect;
import android.hardware.camera2.CameraCharacteristics;
import android.hardware.camera2.CameraManager;
import android.net.Uri;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.net.RouteInfo;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.os.Handler;
import android.os.Looper;
import android.os.PowerManager;
import android.os.ext.SdkExtensions;
import android.provider.MediaStore;
import android.provider.OpenableColumns;
import android.util.Log;
import android.util.DisplayMetrics;
import android.util.TypedValue;
import android.util.Xml;
import android.view.View;
import android.view.ViewGroup;
import android.view.ViewTreeObserver;
import android.view.WindowInsets;
import android.view.Window;

import androidx.core.app.NotificationManagerCompat;
import androidx.core.content.ContextCompat;
import androidx.core.content.FileProvider;
import androidx.core.view.WindowCompat;
import androidx.core.view.WindowInsetsControllerCompat;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;
import org.xmlpull.v1.XmlPullParser;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.Inet4Address;
import java.net.InterfaceAddress;
import java.net.NetworkInterface;
import java.util.Enumeration;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

import dev.makepad.android.MakepadNative;

/**
 * Java side of paseo-gui-platform's Android bridge. Synchronous calls return
 * {@code {"ok": value}} or {@code {"error": {code, message}}}; asynchronous
 * calls report through {@link #nativeResult}; OS events through
 * {@link #nativeEvent}.
 */
final class PaseoBridge {
    static final String TAG = "PaseoBridge";
    static final String AUTHORITY = "sh.paseo.sessions.files";
    static final String PREFERENCES = "paseo_platform";
    static final String EXTRA_PAYLOAD = "paseo.notification.payload";
    /** A {@code paseo://} link forwarded by {@link PaseoLinkActivity}. */
    static final String EXTRA_LINK = "paseo.link";

    static native void nativeResult(long id, String reply);
    static native void nativeEvent(String kind, String payload);
    static native String nativeTailnetNetwork(String snapshot);

    /** Foreground state for the messaging service (silent foreground presentation). */
    static volatile boolean foreground = false;
    /** The live bridge, for token refresh events from the messaging service. */
    static volatile PaseoBridge current;

    private final MainActivity activity;
    private final Handler main = new Handler(Looper.getMainLooper());
    private final ExecutorService io = Executors.newSingleThreadExecutor();
    private final Map<Integer, Long> activityRequests = new HashMap<>();
    private final Map<Integer, PermissionRequest> permissionRequests = new HashMap<>();
    // Do not reuse a destroyed activity's codes: Android may still deliver its
    // outstanding result to the replacement activity.
    private static int nextRequestCode = 0x5000;
    private PaseoAudio audio;
    private String lastInsets = "";
    private boolean systemBarsConfigured;
    private boolean systemBarsDark;
    private int systemBarsBackground;
    private boolean destroyed;
    private ConnectivityManager pathManager;
    private Network pathNetwork;
    private LinkProperties pathProperties;
    // These callbacks can run while the GUI is suspended. Handle them on
    // the bridge's main handler so destruction and path updates are ordered.
    private final ConnectivityManager.NetworkCallback pathCallback = new ConnectivityManager.NetworkCallback() {
        @Override public void onAvailable(Network network) {
            main.post(() -> {
                if (destroyed || current != PaseoBridge.this || network.equals(pathNetwork)) return;
                pathNetwork = network;
                pathProperties = null;
                nativeEvent("networkChanged", "{}");
            });
        }

        @Override public void onLost(Network network) {
            main.post(() -> {
                if (destroyed || current != PaseoBridge.this || !network.equals(pathNetwork)) return;
                pathNetwork = null;
                pathProperties = null;
                nativeEvent("networkChanged", "{}");
            });
        }

        @Override public void onLinkPropertiesChanged(Network network, LinkProperties properties) {
            main.post(() -> {
                if (destroyed || current != PaseoBridge.this
                        || !network.equals(pathNetwork) || properties.equals(pathProperties)) return;
                pathProperties = properties;
                nativeEvent("networkChanged", "{}");
            });
        }
    };
    // Main-thread owned, including completion after an IO task. Keep the request
    // alive while copying so native callers retain the generated source.
    private SaveRequest pendingSave;

    private static final class SaveRequest {
        final long id;
        final File source;
        int requestCode = -1;
        boolean copying;
        boolean preparing = true;

        SaveRequest(long id, File source) {
            this.id = id;
            this.source = source;
        }
    }

    PaseoBridge(MainActivity activity) {
        this.activity = activity;
        current = this;
    }

    /**
     * App-only observation, not a VpnService. Android's supported Java interface
     * API can enumerate app-visible addresses where Go's RTM_GETLINK is denied.
     * Keep one application-context observer across activity recreation, and
     * unregister both callbacks when the embedded node closes.
     */
    private static final class TailnetNetworkMonitor {
        private static TailnetNetworkMonitor current;
        private final ConnectivityManager manager;
        private boolean running;
        private boolean defaultRegistered;
        private boolean interfacesRegistered;
        private Network defaultNetwork;
        private LinkProperties defaultProperties;

        private final ConnectivityManager.NetworkCallback defaultCallback = new ConnectivityManager.NetworkCallback() {
            @Override public void onAvailable(Network network) {
                synchronized (TailnetNetworkMonitor.this) {
                    if (!running) return;
                    if (!network.equals(defaultNetwork)) defaultProperties = null;
                    defaultNetwork = network;
                    changed();
                }
            }
            @Override public void onLost(Network network) {
                synchronized (TailnetNetworkMonitor.this) {
                    if (!running || !network.equals(defaultNetwork)) return;
                    defaultNetwork = null;
                    defaultProperties = null;
                    changed();
                }
            }
            @Override public void onLinkPropertiesChanged(Network network, LinkProperties properties) {
                synchronized (TailnetNetworkMonitor.this) {
                    if (!running || !network.equals(defaultNetwork)) return;
                    // Consume callback data, not racy synchronous CM queries
                    // from inside onAvailable/onLinkPropertiesChanged.
                    defaultProperties = properties;
                    changed();
                }
            }
            @Override public void onCapabilitiesChanged(Network network, NetworkCapabilities capabilities) { changed(); }
        };
        private final ConnectivityManager.NetworkCallback interfacesCallback = callback();

        private ConnectivityManager.NetworkCallback callback() {
            return new ConnectivityManager.NetworkCallback() {
                @Override public void onAvailable(Network network) { changed(); }
                @Override public void onLost(Network network) { changed(); }
                @Override public void onLinkPropertiesChanged(Network network, LinkProperties properties) { changed(); }
                @Override public void onCapabilitiesChanged(Network network, NetworkCapabilities capabilities) { changed(); }
            };
        }

        private TailnetNetworkMonitor(Context context) {
            manager = (ConnectivityManager) context.getSystemService(Context.CONNECTIVITY_SERVICE);
            if (manager == null) throw new IllegalStateException("Android ConnectivityManager is unavailable");
        }

        static synchronized void setEnabled(Context context, boolean enabled) throws Exception {
            if (!enabled) {
                if (current != null) {
                    current.stop();
                    current = null;
                }
                return;
            }
            if (current == null) current = new TailnetNetworkMonitor(context);
            try {
                current.start();
            } catch (Exception error) {
                try { current.stop(); } catch (Exception cleanup) { error.addSuppressed(cleanup); }
                current = null;
                throw error;
            }
        }

        private synchronized void start() throws Exception {
            if (!running) {
                running = true;
                defaultNetwork = manager.getActiveNetwork();
                defaultProperties = defaultNetwork == null ? null : manager.getLinkProperties(defaultNetwork);
                manager.registerDefaultNetworkCallback(defaultCallback);
                defaultRegistered = true;
                // Observe address/interface changes on non-default networks too.
                manager.registerNetworkCallback(new NetworkRequest.Builder().clearCapabilities().build(), interfacesCallback);
                interfacesRegistered = true;
            }
            publish();
        }

        private synchronized void stop() {
            running = false;
            try {
                if (defaultRegistered) {
                    manager.unregisterNetworkCallback(defaultCallback);
                    defaultRegistered = false;
                }
            } finally {
                if (interfacesRegistered) {
                    manager.unregisterNetworkCallback(interfacesCallback);
                    interfacesRegistered = false;
                }
            }
        }

        private synchronized void changed() {
            if (!running) return;
            try {
                publish();
            } catch (Exception error) {
                // publish also forwards the failure to Go: startup/status must
                // fail rather than quietly retaining a stale or empty snapshot.
                Log.e(TAG, "Embedded Tailnet network observation failed", error);
            }
        }

        private void publish() throws Exception {
            String snapshot;
            try {
                snapshot = snapshot().toString();
            } catch (Exception error) {
                nativeTailnetNetwork(new JSONObject().put("error", error.toString()).toString());
                throw error;
            }
            String error = nativeTailnetNetwork(snapshot);
            if (error != null) throw new IllegalStateException(error);
        }

        private JSONObject snapshot() throws Exception {
            JSONArray interfaces = new JSONArray();
            Enumeration<NetworkInterface> all = NetworkInterface.getNetworkInterfaces();
            while (all != null && all.hasMoreElements()) {
                NetworkInterface network = all.nextElement();
                JSONArray addresses = new JSONArray();
                boolean broadcast = false;
                for (InterfaceAddress address : network.getInterfaceAddresses()) {
                    // IPv6 scope belongs to the interface, not the CIDR parser.
                    String ip = address.getAddress().getHostAddress();
                    int scope = ip.indexOf('%');
                    if (scope >= 0) ip = ip.substring(0, scope);
                    addresses.put(ip + "/" + address.getNetworkPrefixLength());
                    broadcast |= address.getBroadcast() != null;
                }
                interfaces.put(new JSONObject()
                        .put("name", network.getName())
                        .put("index", network.getIndex())
                        .put("mtu", network.getMTU())
                        .put("up", network.isUp())
                        .put("loopback", network.isLoopback())
                        .put("point_to_point", network.isPointToPoint())
                        .put("multicast", network.supportsMulticast())
                        .put("broadcast", broadcast)
                        .put("addresses", addresses));
            }
            String defaultInterface = "";
            String defaultGateway = "";
            LinkProperties properties = defaultProperties;
            if (properties != null) {
                String name = properties.getInterfaceName();
                if (name != null) defaultInterface = name;
                for (RouteInfo route : properties.getRoutes()) {
                    if (route.isDefaultRoute() && route.getGateway() instanceof Inet4Address
                            && !route.getGateway().isAnyLocalAddress()) {
                        defaultGateway = route.getGateway().getHostAddress();
                        break;
                    }
                }
            }
            return new JSONObject().put("interfaces", interfaces)
                    .put("default_interface", defaultInterface)
                    .put("default_gateway", defaultGateway);
        }
    }

    synchronized PaseoAudio audio() {
        if (audio == null) audio = new PaseoAudio(activity);
        return audio;
    }

    // ----- lifecycle -----

    /**
     * Installs the safe-area/keyboard observer. The vendored MakepadActivity's
     * own IME listeners carry BuildCutover's API 24 guards
     * (packaging/android/makepad/PATCHES.diff) and are left in place.
     */
    void attachViews() {
        // Host connections need the default path even when Embedded Tailscale is off.
        ConnectivityManager manager = (ConnectivityManager)
                activity.getSystemService(Context.CONNECTIVITY_SERVICE);
        if (manager != null) {
            try {
                pathNetwork = manager.getActiveNetwork();
                pathProperties = pathNetwork == null ? null : manager.getLinkProperties(pathNetwork);
                manager.registerDefaultNetworkCallback(pathCallback);
                pathManager = manager;
            } catch (RuntimeException error) {
                Log.w(TAG, "Cannot observe default network changes", error);
            }
        }
        ViewGroup content = activity.findViewById(android.R.id.content);
        if (content == null) return;
        content.getViewTreeObserver().addOnGlobalLayoutListener(this::publishInsets);
    }

    void onResume() {
        foreground = true;
        if (systemBarsConfigured) applySystemBars();
        // A start refused in the background runs now; a notification hidden for
        // lack of POST_NOTIFICATIONS appears once it was allowed in settings.
        PaseoTailnetService.retry(activity.getApplicationContext());
        PaseoTailnetService.refresh();
        nativeEvent("resumed", "{}");
        publishInsets();
    }

    void onPause() {
        foreground = false;
    }

    void onDestroy() {
        destroyed = true;
        if (pathManager != null) {
            pathManager.unregisterNetworkCallback(pathCallback);
            pathManager = null;
        }
        if (pendingSave != null && !pendingSave.preparing && !pendingSave.copying) {
            finishSave(pendingSave, error("native", "The activity closed before the document was saved."));
        }
        // Preparation and an already-started write own their completion until
        // streams close, even if this activity is destroyed in the meantime.
        if (current == this) current = null;
        if (audio != null) audio.release();
    }

    /** Links forwarded by PaseoLinkActivity and notification taps that started or reached the activity. */
    void onIntent(Intent intent) {
        if (intent == null) return;
        try {
            Bundle extras = intent.getExtras();
            if (extras == null) return;
            String link = extras.getString(EXTRA_LINK);
            if (link != null) {
                nativeEvent("link", new JSONObject().put("url", link).toString());
                intent.removeExtra(EXTRA_LINK);
            }
            String payload = extras.getString(EXTRA_PAYLOAD);
            if (payload != null) {
                JSONObject tap = new JSONObject(payload);
                nativeEvent("notificationTap", tap.toString());
                intent.removeExtra(EXTRA_PAYLOAD);
            } else if (extras.containsKey("google.message_id")) {
                // A system-tray FCM notification tap: the data payload is in the extras.
                JSONObject data = new JSONObject();
                for (String key : extras.keySet()) {
                    if (key.startsWith("google.") || key.startsWith("gcm.") || key.equals("from") || key.equals("collapse_key")) continue;
                    // FCM data payload values are strings.
                    String value = extras.getString(key);
                    if (value != null) data.put(key, value);
                }
                JSONObject tap = new JSONObject()
                        .put("id", extras.getString("google.message_id"))
                        .put("data", data);
                nativeEvent("notificationTap", tap.toString());
                intent.removeExtra("google.message_id");
            }
        } catch (JSONException e) {
            Log.w(TAG, "Invalid launch payload", e);
        }
    }

    @SuppressWarnings("deprecation")
    private void publishInsets() {
        View content = activity.getRenderView();
        if (content == null) return;
        WindowInsets insets = content.getRootWindowInsets();
        if (insets == null) return;
        float density = activity.getResources().getDisplayMetrics().density;
        int top, bottom, left, right, ime;
        if (Build.VERSION.SDK_INT >= 30) {
            Insets bars = insets.getInsets(WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
            top = bars.top; bottom = bars.bottom; left = bars.left; right = bars.right;
            ime = insets.getInsets(WindowInsets.Type.ime()).bottom;
        } else {
            top = insets.getSystemWindowInsetTop();
            bottom = insets.getStableInsetBottom();
            left = insets.getSystemWindowInsetLeft();
            right = insets.getSystemWindowInsetRight();
            ime = Math.max(0, insets.getSystemWindowInsetBottom() - insets.getStableInsetBottom());
        }
        // Insets describe occlusion of the actual native rendering surface. Its
        // parent already consumes IME padding (or adjustResize on older APIs).
        int[] location = new int[2];
        content.getLocationInWindow(location);
        View root = content.getRootView();
        int coveredTop = location[1];
        int coveredBottom = Math.max(0, root.getHeight() - (location[1] + content.getHeight()));
        int coveredLeft = location[0];
        int coveredRight = Math.max(0, root.getWidth() - (location[0] + content.getWidth()));
        try {
            JSONObject value = new JSONObject()
                    .put("top", Math.max(0, top - coveredTop) / density)
                    .put("bottom", Math.max(0, bottom - coveredBottom) / density)
                    .put("left", Math.max(0, left - coveredLeft) / density)
                    .put("right", Math.max(0, right - coveredRight) / density)
                    .put("keyboard", Math.max(0, ime - coveredBottom) / density)
                    .put("keyboardVisible", ime > 0);
            String text = value.toString();
            if (!text.equals(lastInsets)) {
                lastInsets = text;
                nativeEvent("insets", text);
            }
        } catch (JSONException ignored) {
        }
    }

    // ----- synchronous calls -----

    static String ok(Object value) {
        try {
            return new JSONObject().put("ok", value == null ? JSONObject.NULL : value).toString();
        } catch (JSONException e) {
            return "{\"ok\":null}";
        }
    }

    static String error(String code, String message) {
        try {
            return new JSONObject().put("error", new JSONObject().put("code", code).put("message", message == null ? "" : message)).toString();
        } catch (JSONException e) {
            return "{\"error\":{\"code\":\"native\",\"message\":\"\"}}";
        }
    }

    String call(String method, String argsJson) {
        try {
            JSONObject args = new JSONObject(argsJson == null || argsJson.isEmpty() ? "{}" : argsJson);
            switch (method) {
                case "tailnetNetworkMonitor":
                    TailnetNetworkMonitor.setEnabled(activity.getApplicationContext(), args.getBoolean("enabled"));
                    return ok(null);
                case "tailnetPresence": {
                    // Service starts and notification updates belong to the main thread.
                    JSONObject presence = args.isNull("presence") ? null : args.getJSONObject("presence");
                    Context context = activity.getApplicationContext();
                    main.post(() -> PaseoTailnetService.present(context, presence));
                    return ok(null);
                }
                case "paths":
                    return ok(new JSONObject()
                            .put("files", activity.getFilesDir().getAbsolutePath())
                            .put("cache", activity.getCacheDir().getAbsolutePath()));
                case "startServices":
                    main.post(this::publishInsets);
                    return ok(null);
                case "openUrl":
                    return openUrl(args.getString("url"));
                case "openPath":
                    return openPath(new File(args.getString("path")), args.optString("mime", "application/octet-stream"));
                case "copyText":
                    clipboard().setPrimaryClip(ClipData.newPlainText("Paseo", args.getString("text")));
                    return ok(null);
                case "readClipboard":
                    return readClipboard(args.optBoolean("imagesOnly"), new File(args.getString("target")));
                case "copyImage": {
                    Uri uri = FileProvider.getUriForFile(activity, AUTHORITY, new File(args.getString("path")));
                    clipboard().setPrimaryClip(ClipData.newUri(activity.getContentResolver(), "Paseo image", uri));
                    return ok(null);
                }
                case "notify":
                    PaseoNotifications.show(activity, args.getInt("id"), args.optString("title", ""),
                            args.isNull("body") ? null : args.optString("body"), args.getString("payload"),
                            args.optBoolean("sound", true), -1);
                    return ok(null);
                case "setBadge":
                    PaseoNotifications.setBadge(activity, args.optInt("count"));
                    return ok(null);
                case "cameraGeometry": {
                    // Lens facing and sensor orientation per camera id for the QR
                    // preview transform (mobile_scanner's CameraX preview applied the
                    // same rotation and front mirroring).
                    CameraManager cameras = (CameraManager) activity.getSystemService(Context.CAMERA_SERVICE);
                    JSONArray list = new JSONArray();
                    for (String cameraId : cameras.getCameraIdList()) {
                        CameraCharacteristics characteristics = cameras.getCameraCharacteristics(cameraId);
                        Integer facing = characteristics.get(CameraCharacteristics.LENS_FACING);
                        Integer sensor = characteristics.get(CameraCharacteristics.SENSOR_ORIENTATION);
                        if (facing == null) continue;
                        list.put(new JSONObject()
                                .put("id", cameraId)
                                .put("front", facing == CameraCharacteristics.LENS_FACING_FRONT)
                                .put("sensor", sensor == null ? 0 : sensor));
                    }
                    return ok(list);
                }
                case "displayRotation":
                    return ok(ContextCompat.getDisplayOrDefault(activity).getRotation() * 90);
                case "appearance": {
                    int night = activity.getResources().getConfiguration().uiMode & Configuration.UI_MODE_NIGHT_MASK;
                    DisplayMetrics metrics = activity.getResources().getDisplayMetrics();
                    // Keep in sync with appearance::TextScale::SP. applyDimension
                    // uses Android 14's nonlinear converter, and scaledDensity
                    // on earlier releases. Makepad applies density separately.
                    JSONArray textScaleDp = new JSONArray();
                    for (float sp : new float[]{0, 8, 10, 12, 14, 18, 20, 24, 30, 100}) {
                        textScaleDp.put(TypedValue.applyDimension(
                                TypedValue.COMPLEX_UNIT_SP, sp, metrics) / metrics.density);
                    }
                    return ok(new JSONObject()
                            .put("dark", night == Configuration.UI_MODE_NIGHT_YES)
                            .put("language", Locale.getDefault().toLanguageTag())
                            .put("textScaleDp", textScaleDp));
                }
                case "setSystemBars": {
                    boolean dark = args.getBoolean("dark");
                    int background = (int) args.getLong("background");
                    main.post(() -> {
                        systemBarsConfigured = true;
                        systemBarsDark = dark;
                        systemBarsBackground = background;
                        applySystemBars();
                    });
                    return ok(null);
                }
                case "moveTaskToBack":
                    main.post(() -> activity.moveTaskToBack(true));
                    return ok(null);
                case "systemFonts":
                    // Android 7–9 have no AFontMatcher (API 29); Rust reads this instead.
                    return ok(systemFonts());
                case "backgroundRestrictions":
                    return ok(backgroundRestrictions());
                case "takeBackgroundError": {
                    SharedPreferences preferences = activity.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE);
                    String value = preferences.getString("backgroundError", null);
                    if (value != null) preferences.edit().remove("backgroundError").apply();
                    return ok(value);
                }
                default:
                    return error("unsupported", "Unknown Android bridge call " + method);
            }
        } catch (Exception e) {
            return error("native", e.getMessage());
        }
    }

    /**
     * The font families declared in {@code /system/etc/fonts.xml}: named families
     * with their faces, aliases, and all fallback families. Paths are absolute;
     * weights, italic flags, collection indices and
     * {@code <axis>} values are copied as declared.
     */
    static JSONObject systemFonts() throws Exception {
        JSONObject families = new JSONObject();
        JSONObject aliases = new JSONObject();
        JSONArray fallbacks = new JSONArray();
        try (InputStream input = new FileInputStream("/system/etc/fonts.xml")) {
            XmlPullParser parser = Xml.newPullParser();
            parser.setFeature(XmlPullParser.FEATURE_PROCESS_NAMESPACES, false);
            parser.setInput(input, null);
            String familyName = null;
            String languages = null;
            JSONArray fonts = null;
            JSONObject font = null;
            StringBuilder file = null;
            for (int event = parser.getEventType(); event != XmlPullParser.END_DOCUMENT; event = parser.next()) {
                if (event == XmlPullParser.START_TAG) {
                    switch (parser.getName()) {
                        case "family":
                            familyName = parser.getAttributeValue(null, "name");
                            languages = parser.getAttributeValue(null, "lang");
                            fonts = new JSONArray();
                            break;
                        case "font":
                            if (fonts == null) break;
                            font = new JSONObject()
                                    .put("weight", parseIntOr(parser.getAttributeValue(null, "weight"), 400))
                                    .put("italic", "italic".equals(parser.getAttributeValue(null, "style")))
                                    .put("index", parseIntOr(parser.getAttributeValue(null, "index"), 0))
                                    .put("axes", new JSONArray());
                            file = new StringBuilder();
                            break;
                        case "axis":
                            String tag = parser.getAttributeValue(null, "tag");
                            String value = parser.getAttributeValue(null, "stylevalue");
                            if (font != null && tag != null && value != null) {
                                try {
                                    font.getJSONArray("axes").put(new JSONObject().put("tag", tag).put("value", Double.parseDouble(value)));
                                } catch (NumberFormatException ignored) {
                                }
                            }
                            break;
                        case "alias":
                            String name = parser.getAttributeValue(null, "name");
                            String to = parser.getAttributeValue(null, "to");
                            if (name != null && to != null) {
                                aliases.put(name, new JSONObject()
                                        .put("to", to)
                                        .put("weight", parseIntOr(parser.getAttributeValue(null, "weight"), 0)));
                            }
                            break;
                        default:
                            break;
                    }
                } else if (event == XmlPullParser.TEXT && file != null) {
                    file.append(parser.getText());
                } else if (event == XmlPullParser.END_TAG) {
                    if ("font".equals(parser.getName()) && font != null && fonts != null) {
                        String path = file.toString().trim();
                        if (!path.isEmpty()) {
                            fonts.put(font.put("path", path.startsWith("/") ? path : "/system/fonts/" + path));
                        }
                        font = null;
                        file = null;
                    } else if ("family".equals(parser.getName()) && fonts != null) {
                        if (familyName != null && !families.has(familyName)) families.put(familyName, fonts);
                        for (int i = 0; i < fonts.length(); i++) {
                            fallbacks.put(fonts.getJSONObject(i).put("languages", languages));
                        }
                        familyName = null;
                        languages = null;
                        fonts = null;
                    }
                }
            }
        }
        return new JSONObject().put("families", families).put("aliases", aliases).put("fallbacks", fallbacks);
    }


    static int parseIntOr(String text, int fallback) {
        if (text == null) return fallback;
        try {
            return Integer.parseInt(text.trim());
        } catch (NumberFormatException e) {
            return fallback;
        }
    }

    @SuppressWarnings("deprecation")
    private void applySystemBars() {
        Window window = activity.getWindow();
        WindowInsetsControllerCompat controller =
                WindowCompat.getInsetsController(window, window.getDecorView());
        controller.setAppearanceLightStatusBars(!systemBarsDark);
        controller.setAppearanceLightNavigationBars(!systemBarsDark);
        // API 35 draws the status bar over the app; do not change its edge-to-edge
        // policy. Earlier releases need the opaque bar to match the same palette.
        if (Build.VERSION.SDK_INT < 35) window.setStatusBarColor(systemBarsBackground);
        // API 24–25 only support white navigation buttons. On newer versions this
        // colors three-button navigation; gesture navigation remains transparent.
        window.setNavigationBarColor(Build.VERSION.SDK_INT >= 26 ? systemBarsBackground : Color.BLACK);
        if (Build.VERSION.SDK_INT >= 28) window.setNavigationBarDividerColor(systemBarsBackground);
    }

    private ClipboardManager clipboard() {
        return (ClipboardManager) activity.getSystemService(Context.CLIPBOARD_SERVICE);
    }

    private String openUrl(String url) {
        Intent intent = new Intent(Intent.ACTION_VIEW, Uri.parse(url));
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        try {
            activity.startActivity(intent);
            return ok(null);
        } catch (ActivityNotFoundException e) {
            return error("native", "No application can open " + url);
        }
    }

    private String openPath(File file, String mime) {
        Uri uri = FileProvider.getUriForFile(activity, AUTHORITY, file);
        Intent intent = new Intent(Intent.ACTION_VIEW);
        intent.setDataAndType(uri, mime);
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_ACTIVITY_NEW_TASK);
        try {
            activity.startActivity(intent);
            return ok(null);
        } catch (ActivityNotFoundException e) {
            return error("native", "No application can open " + file.getName());
        }
    }

    private String readClipboard(boolean imagesOnly, File target) throws Exception {
        ClipboardManager clipboard = clipboard();
        if (!clipboard.hasPrimaryClip()) return ok(new JSONObject());
        ClipData clip = clipboard.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) return ok(new JSONObject());
        ClipDescription description = clip.getDescription();
        for (int i = 0; i < clip.getItemCount(); i++) {
            Uri uri = clip.getItemAt(i).getUri();
            if (uri == null) continue;
            String type = activity.getContentResolver().getType(uri);
            boolean image = (type != null && type.startsWith("image/")) || description.hasMimeType("image/*");
            if (!image) continue;
            File parent = target.getParentFile();
            if (parent != null) parent.mkdirs();
            File out = new File(parent, "clipboard-image");
            try (InputStream input = activity.getContentResolver().openInputStream(uri);
                 OutputStream output = new FileOutputStream(out)) {
                if (input == null) continue;
                copy(input, output);
            }
            return ok(new JSONObject().put("imagePath", out.getAbsolutePath()));
        }
        if (imagesOnly) return ok(new JSONObject());
        CharSequence text = clip.getItemAt(0).coerceToText(activity);
        return ok(new JSONObject().put("text", text == null ? JSONObject.NULL : text.toString()));
    }

    static void copy(InputStream input, OutputStream output) throws java.io.IOException {
        byte[] buffer = new byte[64 * 1024];
        int read;
        while ((read = input.read(buffer)) >= 0) output.write(buffer, 0, read);
    }

    // ----- asynchronous calls -----

    void callAsync(long id, String method, String argsJson) {
        main.post(() -> {
            try {
                JSONObject args = new JSONObject(argsJson == null || argsJson.isEmpty() ? "{}" : argsJson);
                switch (method) {
                    case "pickFiles":
                        pickFiles(id, args.optBoolean("multiple"), args.optBoolean("imagesOnly"));
                        break;
                    case "saveFile":
                        saveFile(id, new File(args.getString("path")), args.getString("mime"), args.getString("name"));
                        break;
                    case "saveImage":
                        saveImage(id, new File(args.getString("path")), args.getString("mime"), args.getString("name"));
                        break;
                    case "share":
                        share(id, new File(args.getString("path")), args.optString("mime", "application/octet-stream"), args.optString("name"));
                        break;
                    case "permission":
                        permission(id, args.getString("permission"), args.optBoolean("request"));
                        break;
                    case "openPermissionSettings":
                        openPermissionSettings(id, args.getString("target"),
                                args.isNull("channel") ? null : args.getString("channel"));
                        break;
                    case "pushProvisioned":
                        nativeResult(id, ok(PaseoPush.provisioned(activity)));
                        break;
                    case "pushToken":
                        PaseoPush.token(activity, id);
                        break;
                    default:
                        nativeResult(id, error("unsupported", "Unknown Android bridge call " + method));
                }
            } catch (Exception e) {
                nativeResult(id, error("native", e.getMessage()));
            }
        });
    }

    private int requestCode() {
        if (nextRequestCode > 0xffff) throw new IllegalStateException("No activity request codes remain.");
        return nextRequestCode++;
    }

    private void pickFiles(long id, boolean multiple, boolean imagesOnly) {
        if (pendingSave != null) {
            nativeResult(id, error("busy", "A document save is already in progress."));
            return;
        }
        Intent intent = imagesOnly ? imagePicker(multiple) : new Intent(Intent.ACTION_OPEN_DOCUMENT);
        if (!imagesOnly) {
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.setType("*/*");
            intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, multiple);
        }
        int code = requestCode();
        activityRequests.put(code, id);
        try {
            activity.startActivityForResult(intent, code);
        } catch (ActivityNotFoundException e) {
            activityRequests.remove(code);
            nativeResult(id, error("native", "No document picker is available."));
        }
    }

    /**
     * Images come from the system photo picker (Android 13+, or 11+ with its module update), like
     * other apps' "add photo"; older systems offer their gallery apps through GET_CONTENT.
     */
    private Intent imagePicker(boolean multiple) {
        if (photoPickerAvailable()) {
            Intent intent = new Intent(MediaStore.ACTION_PICK_IMAGES);
            if (multiple) {
                intent.putExtra(MediaStore.EXTRA_PICK_IMAGES_MAX, MediaStore.getPickImagesMaxLimit());
            }
            return intent;
        }
        Intent intent = new Intent(Intent.ACTION_GET_CONTENT);
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        intent.setType("image/*");
        intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, multiple);
        return intent;
    }

    private static boolean photoPickerAvailable() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) return true;
        return Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
                && SdkExtensions.getExtensionVersion(Build.VERSION_CODES.R) >= 2;
    }

    /**
     * Saves an image into the gallery (Pictures/Passession) without asking for a location, like
     * "Save image" in other apps. Before Android 10 the shared Pictures folder needs a storage
     * permission, so those systems keep the document save dialog.
     */
    private void saveImage(long id, File source, String mime, String name) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            saveFile(id, source, mime, name);
            return;
        }
        io.execute(() -> {
            ContentResolver resolver = activity.getApplicationContext().getContentResolver();
            Uri destination = null;
            String reply;
            try {
                requireSaveSource(source);
                ContentValues values = new ContentValues();
                values.put(MediaStore.MediaColumns.DISPLAY_NAME, name);
                values.put(MediaStore.MediaColumns.MIME_TYPE, mime);
                values.put(MediaStore.MediaColumns.RELATIVE_PATH, Environment.DIRECTORY_PICTURES + "/Passession");
                values.put(MediaStore.MediaColumns.IS_PENDING, 1);
                destination = resolver.insert(MediaStore.Images.Media.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY), values);
                if (destination == null) throw new java.io.IOException("The gallery did not accept the image.");
                try (InputStream input = new FileInputStream(source);
                     OutputStream output = resolver.openOutputStream(destination, "w")) {
                    if (output == null) throw new java.io.IOException("The gallery could not open the image for writing.");
                    copy(input, output);
                    output.flush();
                }
                values.clear();
                values.put(MediaStore.MediaColumns.IS_PENDING, 0);
                resolver.update(destination, values, null, null);
                reply = ok(true);
            } catch (Exception e) {
                if (destination != null) {
                    try {
                        resolver.delete(destination, null, null);
                    } catch (Exception ignored) {
                        // The failed entry stays pending and is hidden from the gallery.
                    }
                }
                reply = error("io", "Cannot save the image to the gallery: " + e.getMessage());
            }
            nativeResult(id, reply);
        });
    }

    void onActivityResult(int requestCode, int resultCode, Intent data) {
        SaveRequest save = pendingSave;
        if (save != null && save.requestCode == requestCode) {
            // Consume the picker result before starting IO; duplicate results
            // must not start another write or report a second completion.
            save.requestCode = -1;
            if (resultCode == Activity.RESULT_CANCELED) {
                finishSave(save, ok(false));
            } else if (resultCode != Activity.RESULT_OK || data == null
                    || data.getData() == null || !"content".equals(data.getData().getScheme())) {
                finishSave(save, error("native", "The document picker did not return a writable content URI."));
            } else {
                writeDocument(save, data.getData());
            }
            return;
        }
        Long id = activityRequests.remove(requestCode);
        if (id == null) return;
        if (resultCode != Activity.RESULT_OK || data == null) {
            nativeResult(id, ok(new JSONArray()));
            return;
        }
        final List<Uri> uris = new ArrayList<>();
        if (data.getClipData() != null) {
            for (int i = 0; i < data.getClipData().getItemCount(); i++) uris.add(data.getClipData().getItemAt(i).getUri());
        } else if (data.getData() != null) {
            uris.add(data.getData());
        }
        final long request = id;
        // Copies run off the main thread; a picked file is a private copy.
        io.execute(() -> {
            try {
                JSONArray files = new JSONArray();
                for (Uri uri : uris) files.put(copyToCache(uri));
                nativeResult(request, ok(files));
            } catch (Exception e) {
                nativeResult(request, error("io", e.getMessage()));
            }
        });
    }

    private JSONObject copyToCache(Uri uri) throws Exception {
        String name = "file";
        long size = -1;
        try (Cursor cursor = activity.getContentResolver().query(uri, null, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int nameIndex = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                int sizeIndex = cursor.getColumnIndex(OpenableColumns.SIZE);
                if (nameIndex >= 0 && !cursor.isNull(nameIndex)) name = cursor.getString(nameIndex);
                if (sizeIndex >= 0 && !cursor.isNull(sizeIndex)) size = cursor.getLong(sizeIndex);
            }
        }
        String safe = name.replaceAll("[<>:\"/\\\\|?*\\x00-\\x1f]", "_");
        File directory = new File(new File(activity.getCacheDir(), "paseo-files"), "transfer-" + UUID.randomUUID().toString().replace("-", ""));
        if (!directory.mkdirs() && !directory.isDirectory()) throw new java.io.IOException("Cannot create " + directory);
        File out = new File(directory, safe.isEmpty() ? "file" : safe);
        try (InputStream input = activity.getContentResolver().openInputStream(uri);
             OutputStream output = new FileOutputStream(out)) {
            if (input == null) throw new java.io.IOException("Cannot read " + name);
            copy(input, output);
        }
        return new JSONObject()
                .put("path", out.getAbsolutePath())
                .put("name", name)
                .put("mimeType", activity.getContentResolver().getType(uri))
                .put("size", size >= 0 ? size : out.length());
    }

    private void saveFile(long id, File source, String mime, String name) {
        if (destroyed || activity.isFinishing() || activity.isDestroyed()) {
            nativeResult(id, error("native", "The activity is not available for a document save."));
            return;
        }
        if (pendingSave != null || !activityRequests.isEmpty()) {
            nativeResult(id, error("busy", "Another document request is already in progress."));
            return;
        }
        SaveRequest save = new SaveRequest(id, source);
        pendingSave = save;
        try {
            io.execute(() -> {
                String failure = null;
                try {
                    requireSaveSource(source);
                    // Check that the file can actually be opened before asking
                    // the user to create a destination.
                    try (InputStream input = new FileInputStream(source)) {
                        // Opening the source is the readability check.
                    }
                } catch (Exception e) {
                    failure = error("io", "Cannot read the document source: " + e.getMessage());
                }
                final String reply = failure;
                main.post(() -> {
                    if (pendingSave != save) return;
                    save.preparing = false;
                    if (reply != null) {
                        finishSave(save, reply);
                        return;
                    }
                    if (destroyed || activity.isFinishing() || activity.isDestroyed()) {
                        finishSave(save, error("native", "The activity closed before the document could be saved."));
                        return;
                    }
                    try {
                        Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT);
                        intent.addCategory(Intent.CATEGORY_OPENABLE);
                        intent.setType(mime);
                        intent.putExtra(Intent.EXTRA_TITLE, name);
                        save.requestCode = requestCode();
                        activity.startActivityForResult(intent, save.requestCode);
                    } catch (Exception e) {
                        finishSave(save, error("native", "Cannot open the document save dialog: " + e.getMessage()));
                    }
                });
            });
        } catch (Exception e) {
            finishSave(save, error("io", "Cannot prepare the document save: " + e.getMessage()));
        }
    }

    private static void requireSaveSource(File source) throws java.io.IOException {
        if (!source.isFile()) {
            throw new java.io.IOException("The source is not a regular file.");
        }
    }

    private void writeDocument(SaveRequest save, Uri destination) {
        save.copying = true;
        try {
            io.execute(() -> {
                String reply;
                try {
                    requireSaveSource(save.source);
                    try (InputStream input = new FileInputStream(save.source);
                         OutputStream output = activity.getApplicationContext().getContentResolver()
                                 .openOutputStream(destination, "wt")) {
                        if (output == null) throw new java.io.IOException("The document provider could not open the destination.");
                        copy(input, output);
                        output.flush();
                    }
                    // Close errors (including provider failures) are caught
                    // above; dispatch success only after both streams close.
                    reply = ok(true);
                } catch (Exception e) {
                    reply = error("io", "Cannot save the document: " + e.getMessage());
                }
                final String result = reply;
                main.post(() -> finishSave(save, result));
            });
        } catch (Exception e) {
            finishSave(save, error("io", "Cannot start the document write: " + e.getMessage()));
        }
    }

    private void finishSave(SaveRequest save, String reply) {
        if (pendingSave != save) return;
        pendingSave = null;
        nativeResult(save.id, reply);
    }

    private void share(long id, File file, String mime, String name) {
        Uri uri = FileProvider.getUriForFile(activity, AUTHORITY, file);
        Intent send = new Intent(Intent.ACTION_SEND);
        send.setType(mime);
        send.putExtra(Intent.EXTRA_STREAM, uri);
        send.putExtra(Intent.EXTRA_TITLE, name);
        send.setClipData(ClipData.newRawUri(name, uri));
        send.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        try {
            activity.startActivity(Intent.createChooser(send, null));
            nativeResult(id, ok(null));
        } catch (ActivityNotFoundException e) {
            nativeResult(id, error("native", "No application can receive " + name));
        }
    }

    /** Set once the OS prompt was requested for a runtime permission. */
    private static String askedKey(String permission) {
        return "permissionAsked." + permission;
    }

    /**
     * {@code "granted"}, {@code "notDetermined"} (never asked: the OS prompt will
     * show), {@code "denied"} (asking again still prompts) or {@code "blocked"}
     * (only system settings can change it).
     */
    private void permission(long id, String kind, boolean request) {
        String permission;
        if ("camera".equals(kind)) {
            permission = Manifest.permission.CAMERA;
        } else if (Build.VERSION.SDK_INT >= 33) {
            permission = Manifest.permission.POST_NOTIFICATIONS;
        } else {
            // Below API 33 notifications need no runtime permission; only system
            // settings can turn them back on.
            boolean enabled = NotificationManagerCompat.from(activity).areNotificationsEnabled();
            nativeResult(id, ok(enabled ? "granted" : "blocked"));
            return;
        }
        if (activity.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED) {
            nativeResult(id, ok("granted"));
            return;
        }
        SharedPreferences preferences = activity.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE);
        if (!request) {
            nativeResult(id, ok(deniedState(permission, preferences.getBoolean(askedKey(permission), false))));
            return;
        }
        // A blocked permission is still requested: Android answers at once without
        // a prompt, and a reset in system settings ("Ask every time") prompts again.
        preferences.edit().putBoolean(askedKey(permission), true).apply();
        int code = requestCode();
        permissionRequests.put(code, new PermissionRequest(id, permission));
        activity.requestPermissions(new String[]{permission}, code);
    }

    /** A permission not granted: Android shows a rationale only while it still prompts. */
    private String deniedState(String permission, boolean asked) {
        if (!asked) return "notDetermined";
        return activity.shouldShowRequestPermissionRationale(permission) ? "denied" : "blocked";
    }

    void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        PermissionRequest request = permissionRequests.remove(requestCode);
        if (request == null) return;
        if (grantResults.length == 0) {
            // Interrupted before the user chose: nothing was decided.
            activity.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE).edit()
                    .remove(askedKey(request.permission)).apply();
            nativeResult(request.id, ok("notDetermined"));
            return;
        }
        boolean granted = grantResults[0] == PackageManager.PERMISSION_GRANTED;
        if (granted) PaseoTailnetService.refresh();
        nativeResult(request.id, ok(granted ? "granted" : deniedState(request.permission, true)));
    }

    /**
     * Opens the system settings screen for {@code target}: {@code notifications}
     * (one {@code channel} when given and created), {@code app} (app details,
     * where camera access lives) or {@code batteryOptimization}.
     */
    private void openPermissionSettings(long id, String target, String channel) {
        String packageName = activity.getPackageName();
        Intent details = new Intent(android.provider.Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                Uri.fromParts("package", packageName, null));
        Intent intent = details;
        if ("notifications".equals(target) && Build.VERSION.SDK_INT >= 26) {
            NotificationManager notifications = activity.getSystemService(NotificationManager.class);
            if (channel != null && notifications.getNotificationChannel(channel) != null) {
                intent = new Intent(android.provider.Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS)
                        .putExtra(android.provider.Settings.EXTRA_APP_PACKAGE, packageName)
                        .putExtra(android.provider.Settings.EXTRA_CHANNEL_ID, channel);
            } else {
                intent = new Intent(android.provider.Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                        .putExtra(android.provider.Settings.EXTRA_APP_PACKAGE, packageName);
            }
        } else if ("batteryOptimization".equals(target)) {
            intent = new Intent(android.provider.Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS);
        }
        try {
            activity.startActivity(intent);
        } catch (ActivityNotFoundException e) {
            // Some vendor builds omit the precise screen; app details link to all of them.
            try {
                activity.startActivity(details);
            } catch (ActivityNotFoundException missing) {
                nativeResult(id, error("native", "System settings are not available."));
                return;
            }
        }
        nativeResult(id, ok(true));
    }

    /** Power management that may stop the app (and Embedded Tailscale) in the background. */
    private JSONObject backgroundRestrictions() throws JSONException {
        PowerManager power = activity.getSystemService(PowerManager.class);
        boolean restricted = Build.VERSION.SDK_INT >= 28
                && activity.getSystemService(ActivityManager.class).isBackgroundRestricted();
        return new JSONObject()
                .put("batteryOptimized", !power.isIgnoringBatteryOptimizations(activity.getPackageName()))
                .put("backgroundRestricted", restricted);
    }

    private static final class PermissionRequest {
        final long id;
        final String permission;

        PermissionRequest(long id, String permission) {
            this.id = id;
            this.permission = permission;
        }
    }
}
