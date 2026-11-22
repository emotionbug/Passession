package sh.paseo.sessions;

import android.app.Activity;
import android.content.ClipData;
import android.content.ContentResolver;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.res.AssetFileDescriptor;
import android.graphics.BitmapFactory;
import android.net.Uri;
import android.os.Bundle;
import android.os.CancellationSignal;
import android.os.Handler;
import android.os.Looper;
import android.os.Process;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.UUID;

/** The Android Sharesheet target. Read grants stay with this short-lived activity. */
public final class PaseoShareActivity extends Activity {
    static final String EXTRA_SHARE = "paseo.inbound.share";
    static final String EXTRA_SHARE_ERROR = "paseo.inbound.share.error";
    private static final int MAX_IMAGES = 8;
    private static final long MAX_IMAGE_BYTES = 12L * 1024 * 1024;
    private static final long MAX_BATCH_BYTES = 24L * 1024 * 1024;
    private static final long MAX_PIXELS = 32_000_000L;
    private static final int MAX_EDGE = 8192;
    private static final String STAGING = "paseo-inbound-share";
    private static final Handler MAIN = new Handler(Looper.getMainLooper());

    // Main-thread state. Only one private batch may exist, including while Rust owns it.
    private static boolean copying;
    private static boolean swept;
    private static Pending pending;
    private final CancellationSignal cancellation = new CancellationSignal();

    private static final class Pending {
        final String token = UUID.randomUUID().toString();
        final File directory;
        final String payload;

        Pending(File directory, String payload) {
            this.directory = directory;
            this.payload = payload;
        }
    }

    private static final class Rejected extends Exception {
        final String code;

        Rejected(String code) {
            this.code = code;
        }
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        final List<Uri> uris;
        try {
            uris = sharedUris(getIntent());
            for (Uri uri : uris) {
                if (checkUriPermission(uri, Process.myPid(), Process.myUid(),
                        Intent.FLAG_GRANT_READ_URI_PERMISSION) != PackageManager.PERMISSION_GRANTED) {
                    throw new Rejected("unreadable");
                }
            }
        } catch (Rejected error) {
            showError(error.code);
            return;
        } catch (RuntimeException error) {
            showError("invalid");
            return;
        }
        if (copying) {
            showError("busy");
            return;
        }
        copying = true;
        Context context = getApplicationContext();
        recoverCache(context);
        // A provider read can block; never run it on the activity or native UI thread.
        new Thread(() -> stage(context, uris), "paseo-inbound-share").start();
    }

    @Override
    protected void onDestroy() {
        cancellation.cancel();
        super.onDestroy();
    }

    @SuppressWarnings("deprecation")
    private static List<Uri> sharedUris(Intent source) throws Rejected {
        String action = source.getAction();
        boolean multiple = Intent.ACTION_SEND_MULTIPLE.equals(action);
        String mime = source.getType();
        if ((!multiple && !Intent.ACTION_SEND.equals(action)) || mime == null
                || !mime.startsWith("image/")) {
            throw new Rejected("invalid");
        }
        if ((source.getFlags() & Intent.FLAG_GRANT_READ_URI_PERMISSION) == 0) {
            throw new Rejected("unreadable");
        }
        List<?> items;
        if (source.hasExtra(Intent.EXTRA_STREAM)) {
            if (multiple) {
                items = source.getParcelableArrayListExtra(Intent.EXTRA_STREAM);
            } else {
                Object stream = source.getParcelableExtra(Intent.EXTRA_STREAM);
                if (!(stream instanceof Uri)) throw new Rejected("invalid");
                items = Collections.singletonList(stream);
            }
        } else {
            ClipData clip = source.getClipData();
            if (clip == null || clip.getItemCount() == 0) throw new Rejected("invalid");
            if (clip.getItemCount() > MAX_IMAGES) throw new Rejected("limits");
            List<Uri> clips = new ArrayList<>(clip.getItemCount());
            for (int i = 0; i < clip.getItemCount(); i++) clips.add(clip.getItemAt(i).getUri());
            items = clips;
        }
        if (items == null || items.isEmpty()) throw new Rejected("invalid");
        if (items.size() > MAX_IMAGES) throw new Rejected("limits");
        if (!multiple && items.size() != 1) throw new Rejected("invalid");
        LinkedHashSet<Uri> uris = new LinkedHashSet<>();
        for (Object item : items) {
            if (!(item instanceof Uri) || !ContentResolver.SCHEME_CONTENT.equals(((Uri) item).getScheme())
                    || ((Uri) item).getAuthority() == null || ((Uri) item).getAuthority().isEmpty()) {
                throw new Rejected("invalid");
            }
            uris.add((Uri) item);
        }
        return new ArrayList<>(uris);
    }

    private void stage(Context context, List<Uri> uris) {
        File root = new File(context.getCacheDir(), STAGING);
        File directory = new File(root, "batch-" + UUID.randomUUID());
        boolean owned = false;
        Pending result = null;
        String error = null;
        try {
            if (!root.mkdir() && !root.isDirectory()) throw new Rejected("unreadable");
            String[] batches = root.list();
            if (batches == null) throw new Rejected("unreadable");
            if (batches.length > 0) throw new Rejected("busy");
            if (!directory.mkdir()) throw new Rejected("unreadable");
            owned = true;
            long remaining = MAX_BATCH_BYTES;
            byte[] buffer = new byte[64 * 1024];
            JSONArray files = new JSONArray();
            ContentResolver resolver = context.getContentResolver();
            for (Uri uri : uris) {
                File file = new File(directory, "image-" + (files.length() + 1));
                long size = copy(resolver, uri, file, Math.min(MAX_IMAGE_BYTES, remaining), buffer);
                remaining -= size;
                BitmapFactory.Options bounds = new BitmapFactory.Options();
                bounds.inJustDecodeBounds = true;
                BitmapFactory.decodeFile(file.getAbsolutePath(), bounds);
                String extension = extension(bounds.outMimeType);
                if (extension == null || bounds.outWidth <= 0 || bounds.outHeight <= 0) {
                    throw new Rejected("unsupported");
                }
                if (bounds.outWidth > MAX_EDGE || bounds.outHeight > MAX_EDGE
                        || (long) bounds.outWidth * bounds.outHeight > MAX_PIXELS) {
                    throw new Rejected("limits");
                }
                files.put(new JSONObject()
                        .put("mimeType", bounds.outMimeType)
                        .put("name", file.getName() + extension)
                        .put("size", size));
            }
            result = new Pending(directory, new JSONObject()
                    .put("directory", directory.getAbsolutePath())
                    .put("files", files).toString());
        } catch (Rejected rejected) {
            error = rejected.code;
        } catch (IOException | JSONException | RuntimeException failure) {
            error = "unreadable";
        } finally {
            if (owned && result == null) remove(directory);
            final Pending ready = result;
            final String code = error;
            MAIN.post(() -> finishCopy(ready, code));
        }
    }

    private long copy(ContentResolver resolver, Uri uri, File file, long limit, byte[] buffer)
            throws IOException, Rejected {
        cancellation.throwIfCanceled();
        try (AssetFileDescriptor descriptor = resolver.openAssetFileDescriptor(uri, "r", cancellation)) {
            if (descriptor == null) throw new Rejected("unreadable");
            if (descriptor.getLength() > limit) throw new Rejected("limits");
            try (InputStream input = descriptor.createInputStream();
                 FileOutputStream output = new FileOutputStream(file)) {
                long total = 0;
                int count;
                while ((count = input.read(buffer, 0, (int) Math.min(buffer.length, limit - total + 1))) != -1) {
                    cancellation.throwIfCanceled();
                    if (count == 0) throw new Rejected("unreadable");
                    if (count > limit - total) throw new Rejected("limits");
                    output.write(buffer, 0, count);
                    total += count;
                }
                if (total == 0) throw new Rejected("unsupported");
                return total;
            }
        }
    }

    private static String extension(String mime) {
        if (mime == null) return null;
        switch (mime) {
            case "image/png": return ".png";
            case "image/jpeg": return ".jpg";
            case "image/gif": return ".gif";
            case "image/webp": return ".webp";
            case "image/bmp":
            case "image/x-ms-bmp": return ".bmp";
            case "image/heic":
            case "image/heic-sequence": return ".heic";
            case "image/heif":
            case "image/heif-sequence": return ".heif";
            case "image/avif": return ".avif";
            default: return null;
        }
    }

    private void finishCopy(Pending ready, String error) {
        copying = false;
        if (isFinishing() || isDestroyed()) {
            if (ready != null) remove(ready.directory);
            return;
        }
        if (ready == null) {
            showError(error);
            return;
        }
        pending = ready;
        MAIN.postDelayed(() -> {
            if (pending == ready) {
                pending = null;
                remove(ready.directory);
            }
        }, 60_000);
        // Only a one-time token reaches MainActivity. No grant or supplier URI leaves this activity.
        try {
            startActivity(PaseoNotifications.openApp(this).putExtra(EXTRA_SHARE, ready.token));
        } catch (RuntimeException failure) {
            pending = null;
            remove(ready.directory);
        } finally {
            finish();
        }
    }

    private void showError(String error) {
        try {
            startActivity(PaseoNotifications.openApp(this).putExtra(EXTRA_SHARE_ERROR, error));
        } catch (RuntimeException ignored) {
            // Android rejected the forward; there is no copied batch to keep.
        } finally {
            finish();
        }
    }

    static String take(String token) {
        if (pending == null || !pending.token.equals(token)) return null;
        String payload = pending.payload;
        pending = null;
        return payload;
    }

    static void recoverCache(Context context) {
        if (swept) return;
        swept = true;
        // Process death can skip the Rust lease destructor; this private
        // directory contains at most one batch and no supplier URIs.
        File root = new File(context.getCacheDir(), STAGING);
        File[] batches = root.listFiles();
        if (batches != null) for (File batch : batches) remove(batch);
        root.delete();
    }

    private static void remove(File directory) {
        File[] files = directory.listFiles();
        if (files != null) for (File file : files) file.delete();
        directory.delete();
    }
}
