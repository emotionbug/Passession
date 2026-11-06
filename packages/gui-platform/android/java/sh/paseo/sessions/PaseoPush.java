package sh.paseo.sessions;

import android.content.Context;
import android.util.Log;

import com.google.firebase.FirebaseApp;
import com.google.firebase.messaging.FirebaseMessaging;

/** Optional Firebase provisioning and FCM registration tokens. */
final class PaseoPush {
    private PaseoPush() {}

    /**
     * Builds without google-services.json cannot issue push tokens; that is a
     * packaging fact, not a runtime failure.
     */
    static boolean provisioned(Context context) {
        try {
            if (!FirebaseApp.getApps(context).isEmpty()) return true;
            return FirebaseApp.initializeApp(context) != null;
        } catch (Throwable error) {
            Log.i(PaseoBridge.TAG, "Firebase is not provisioned: " + error);
            return false;
        }
    }

    /** The FCM registration token Expo exchanges (Messaging 24 `getToken`). */
    static void token(Context context, long id) {
        try {
            FirebaseMessaging.getInstance().getToken().addOnCompleteListener(task -> {
                if (task.isSuccessful() && task.getResult() != null) {
                    PaseoBridge.nativeResult(id, PaseoBridge.ok(task.getResult()));
                } else {
                    Exception error = task.getException();
                    PaseoBridge.nativeResult(id, PaseoBridge.error("push_token", error == null ? "No push token was issued." : error.getMessage()));
                }
            });
        } catch (Throwable error) {
            PaseoBridge.nativeResult(id, PaseoBridge.error("push_token", error.getMessage()));
        }
    }
}
