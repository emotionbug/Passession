package sh.paseo.sessions;

import android.content.Context;
import android.content.SharedPreferences;

import com.google.firebase.messaging.FirebaseMessagingService;
import com.google.firebase.messaging.RemoteMessage;

import org.json.JSONObject;

import java.util.Map;

/**
 * FCM delivery (original mobile_notification_delivery.dart): foreground
 * messages are presented silently; background data-only messages are presented
 * with sound. Messages with an OS-rendered notification are left to the system
 * while in the background. Failures are kept for the UI to report on resume.
 */
public class PaseoMessagingService extends FirebaseMessagingService {
    @Override
    public void onMessageReceived(RemoteMessage message) {
        boolean foreground = PaseoBridge.foreground;
        if (!foreground && message.getNotification() != null) return;
        try {
            Map<String, String> raw = message.getData();
            String title = message.getNotification() != null ? message.getNotification().getTitle() : raw.get("title");
            String body = message.getNotification() != null ? message.getNotification().getBody() : raw.get("message");
            if (title == null && body == null) return;
            String identity = message.getMessageId() != null
                    ? message.getMessageId()
                    : message.getSentTime() + ":" + new JSONObject(raw);
            int id = 0;
            for (int i = 0; i < identity.length(); i++) id = (id * 31 + identity.charAt(i)) & 0x7fffffff;
            JSONObject payload = new JSONObject().put("id", identity).put("data", applicationData(raw));
            int number = -1;
            try {
                if (raw.get("badge") != null) number = Integer.parseInt(raw.get("badge"));
            } catch (NumberFormatException ignored) {
            }
            PaseoNotifications.show(this, id, title, body, payload.toString(), !foreground, number);
        } catch (Exception error) {
            SharedPreferences preferences = getSharedPreferences(PaseoBridge.PREFERENCES, Context.MODE_PRIVATE);
            preferences.edit().putString("backgroundError", String.valueOf(error.getMessage())).apply();
        }
    }

    /** Expo's FCM transport stores application data as JSON in `body`. */
    static JSONObject applicationData(Map<String, String> raw) {
        String body = raw.get("body");
        if (body != null) {
            try {
                return new JSONObject(body);
            } catch (Exception ignored) {
                // Native messages expose application data directly instead.
            }
        }
        return new JSONObject(raw);
    }

    @Override
    public void onNewToken(String token) {
        if (PaseoBridge.current != null) PaseoBridge.nativeEvent("tokenRefreshed", "{}");
    }
}
