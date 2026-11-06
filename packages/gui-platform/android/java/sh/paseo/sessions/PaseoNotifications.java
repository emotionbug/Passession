package sh.paseo.sessions;

import android.Manifest;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.Build;

import androidx.core.app.NotificationCompat;
import androidx.core.app.NotificationManagerCompat;

/** Local notification presentation shared by the bridge and the messaging service. */
final class PaseoNotifications {
    static final String CHANNEL = "default";

    private PaseoNotifications() {}

    /** Original createDefaultNotificationChannel: id/name "default", default importance. */
    static void ensureChannel(Context context) {
        if (Build.VERSION.SDK_INT >= 26) {
            NotificationManager manager = context.getSystemService(NotificationManager.class);
            if (manager != null && manager.getNotificationChannel(CHANNEL) == null) {
                manager.createNotificationChannel(new NotificationChannel(CHANNEL, CHANNEL, NotificationManager.IMPORTANCE_DEFAULT));
            }
        }
    }

    /**
     * Shows one notification. A tap opens MainActivity with {@code payload}
     * (JSON {@code {id, data}}) in {@link PaseoBridge#EXTRA_PAYLOAD}.
     * {@code number} below zero leaves the badge number unset.
     */
    static void show(Context context, int id, String title, String body, String payload, boolean sound, int number) {
        if (Build.VERSION.SDK_INT >= 33
                && context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            throw new IllegalStateException("Notification permission is not granted.");
        }
        ensureChannel(context);
        Intent intent = new Intent(context, MainActivity.class);
        intent.setFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        intent.putExtra(PaseoBridge.EXTRA_PAYLOAD, payload);
        int flags = PendingIntent.FLAG_UPDATE_CURRENT | (Build.VERSION.SDK_INT >= 23 ? PendingIntent.FLAG_IMMUTABLE : 0);
        PendingIntent tap = PendingIntent.getActivity(context, id, intent, flags);
        NotificationCompat.Builder builder = new NotificationCompat.Builder(context, CHANNEL)
                .setSmallIcon(R.drawable.paseo_notification_icon)
                .setContentTitle(title)
                .setContentText(body)
                .setPriority(NotificationCompat.PRIORITY_DEFAULT)
                .setAutoCancel(true)
                .setContentIntent(tap)
                .setSilent(!sound);
        if (!sound) builder.setVibrate(null);
        if (number >= 0) builder.setNumber(number);
        NotificationManagerCompat.from(context).notify(id, builder.build());
    }

    /** mobile_services.dart setBadgeCount: {@code if (isSupported()) updateBadge(count)}. */
    static void setBadge(Context context, int count) {
        if (PaseoLauncherBadge.isSupported(context)) PaseoLauncherBadge.update(context, count);
    }
}
