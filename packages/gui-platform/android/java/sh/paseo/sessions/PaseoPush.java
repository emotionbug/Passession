package sh.paseo.sessions;

import android.content.Context;

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
        // initializeApp returns the existing default app, or null when the
        // build has no Firebase resources.
        return FirebaseApp.initializeApp(context) != null;
    }

    /** The FCM registration token Expo exchanges (Messaging 24 `getToken`). */
    static void token(Context context, long id) {
        try {
            if (!provisioned(context)) {
                PaseoBridge.nativeResult(id, PaseoBridge.error("push_unavailable", "Push notifications are not available in this build."));
                return;
            }
            FirebaseMessaging.getInstance().getToken().addOnCompleteListener(task -> {
                if (task.isSuccessful() && task.getResult() != null) {
                    PaseoBridge.nativeResult(id, PaseoBridge.ok(task.getResult()));
                } else {
                    Exception error = task.getException();
                    String message = error == null ? null : error.getMessage();
                    boolean transientFailure = "SERVICE_NOT_AVAILABLE".equals(message)
                            || "INTERNAL_SERVER_ERROR".equals(message) || "InternalServerError".equals(message);
                    PaseoBridge.nativeResult(id, PaseoBridge.error(
                            transientFailure ? "push_retryable" : "push_token",
                            message == null ? "No push token was issued." : message));
                }
            });
        } catch (Throwable error) {
            PaseoBridge.nativeResult(id, PaseoBridge.error("push_token", error.getMessage()));
        }
    }
}
