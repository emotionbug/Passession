package sh.paseo.sessions;

import android.Manifest;
import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.ClipDescription;
import android.content.ClipboardManager;
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
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.OpenableColumns;
import android.util.Log;
import android.util.DisplayMetrics;
import android.util.TypedValue;
import android.view.View;
import android.view.ViewGroup;
import android.view.ViewTreeObserver;
import android.view.WindowInsets;
import android.view.Window;

import androidx.core.app.NotificationManagerCompat;
import androidx.core.content.FileProvider;
import androidx.core.view.WindowCompat;
import androidx.core.view.WindowInsetsControllerCompat;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
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

    static native void nativeResult(long id, String reply);
    static native void nativeEvent(String kind, String payload);

    /** Foreground state for the messaging service (silent foreground presentation). */
    static volatile boolean foreground = false;
    /** The live bridge, for token refresh events from the messaging service. */
    static volatile PaseoBridge current;

    private final MainActivity activity;
    private final Handler main = new Handler(Looper.getMainLooper());
    private final ExecutorService io = Executors.newSingleThreadExecutor();
    private final Map<Integer, Long> activityRequests = new HashMap<>();
    private final Map<Integer, Long> permissionRequests = new HashMap<>();
    private int nextRequestCode = 0x5000;
    private PaseoAudio audio;
    private String lastInsets = "";
    private boolean systemBarsConfigured;
    private boolean systemBarsDark;
    private int systemBarsBackground;

    PaseoBridge(MainActivity activity) {
        this.activity = activity;
        current = this;
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
        ViewGroup content = activity.findViewById(android.R.id.content);
        if (content == null) return;
        content.getViewTreeObserver().addOnGlobalLayoutListener(this::publishInsets);
    }

    void onResume() {
        foreground = true;
        if (systemBarsConfigured) applySystemBars();
        nativeEvent("resumed", "{}");
        publishInsets();
    }

    void onPause() {
        foreground = false;
    }

    void onDestroy() {
        if (current == this) current = null;
        if (audio != null) audio.release();
    }

    /** OS links and notification taps that started or reached the activity. */
    void onIntent(Intent intent) {
        if (intent == null) return;
        try {
            Uri link = intent.getData();
            if (Intent.ACTION_VIEW.equals(intent.getAction()) && link != null) {
                nativeEvent("link", new JSONObject().put("url", link.toString()).toString());
            }
            Bundle extras = intent.getExtras();
            if (extras == null) return;
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
                    Object value = extras.get(key);
                    if (value instanceof String) data.put(key, value);
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
                    return ok(activity.getWindowManager().getDefaultDisplay().getRotation() * 90);
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
                    case "share":
                        share(id, new File(args.getString("path")), args.optString("mime", "application/octet-stream"), args.optString("name"));
                        break;
                    case "permission":
                        permission(id, args.getString("permission"), args.optBoolean("request"));
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
        return nextRequestCode++;
    }

    private void pickFiles(long id, boolean multiple, boolean imagesOnly) {
        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        intent.setType(imagesOnly ? "image/*" : "*/*");
        intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, multiple);
        int code = requestCode();
        activityRequests.put(code, id);
        try {
            activity.startActivityForResult(intent, code);
        } catch (ActivityNotFoundException e) {
            activityRequests.remove(code);
            nativeResult(id, error("native", "No document picker is available."));
        }
    }

    void onActivityResult(int requestCode, int resultCode, Intent data) {
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

    private void permission(long id, String kind, boolean request) {
        String permission;
        if ("camera".equals(kind)) {
            permission = Manifest.permission.CAMERA;
        } else if (Build.VERSION.SDK_INT >= 33) {
            permission = Manifest.permission.POST_NOTIFICATIONS;
        } else {
            // Below API 33 notifications need no runtime permission; the user
            // may still have disabled them in system settings.
            boolean enabled = NotificationManagerCompat.from(activity).areNotificationsEnabled();
            nativeResult(id, ok(enabled ? "granted" : "denied"));
            return;
        }
        if (activity.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED) {
            nativeResult(id, ok("granted"));
            return;
        }
        if (!request) {
            nativeResult(id, ok("denied"));
            return;
        }
        int code = requestCode();
        permissionRequests.put(code, id);
        activity.requestPermissions(new String[]{permission}, code);
    }

    void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        Long id = permissionRequests.remove(requestCode);
        if (id == null) return;
        boolean granted = grantResults.length > 0 && grantResults[0] == PackageManager.PERMISSION_GRANTED;
        nativeResult(id, ok(granted ? "granted" : "denied"));
    }
}
