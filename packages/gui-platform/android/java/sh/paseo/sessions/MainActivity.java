package sh.paseo.sessions;

import android.content.Intent;
import android.content.res.Configuration;
import android.os.Build;
import android.os.Bundle;
import android.window.OnBackInvokedCallback;
import android.window.OnBackInvokedDispatcher;

import dev.makepad.android.MakepadActivity;
import dev.makepad.android.MakepadNative;

/**
 * Launcher activity: the unmodified Makepad activity plus the Paseo platform
 * bridge (paseo-gui-platform `android` module). Rust calls the public
 * {@code paseo*} methods through JNI.
 */
public class MainActivity extends MakepadActivity {
    private PaseoBridge bridge;
    private OnBackInvokedCallback backCallback;

    @Override
    public void onCreate(Bundle savedInstanceState) {
        // Rust may call into the bridge from the Makepad startup that
        // super.onCreate triggers, so it exists first.
        bridge = new PaseoBridge(this);
        super.onCreate(savedInstanceState);
        bridge.attachViews();
        bridge.onIntent(getIntent());
        if (Build.VERSION.SDK_INT >= 33) {
            // DEFAULT leaves the IME's higher-priority dismissal ahead of app navigation.
            backCallback = () -> {
                if (!dismissTextActionsOnBack()) MakepadNative.onBackPressed();
            };
            getOnBackInvokedDispatcher().registerOnBackInvokedCallback(
                    OnBackInvokedDispatcher.PRIORITY_DEFAULT, backCallback);
        }
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        bridge.onIntent(intent);
    }

    @Override
    protected void onResume() {
        super.onResume();
        bridge.onResume();
    }

    @Override
    public void onConfigurationChanged(Configuration configuration) {
        super.onConfigurationChanged(configuration);
        PaseoBridge.nativeEvent("appearanceChanged", "{}");
    }

    @Override
    protected void onPause() {
        bridge.onPause();
        super.onPause();
    }

    @Override
    protected void onDestroy() {
        if (Build.VERSION.SDK_INT >= 33 && backCallback != null) {
            getOnBackInvokedDispatcher().unregisterOnBackInvokedCallback(backCallback);
            backCallback = null;
        }
        bridge.onDestroy();
        super.onDestroy();
    }

    /**
     * Back goes to the shell first. Makepad's default also finishes the
     * activity; here the shell's unhandled Back moves the task to the
     * background instead (remote sessions keep running).
     */
    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        if (!dismissTextActionsOnBack()) MakepadNative.onBackPressed();
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        bridge.onActivityResult(requestCode, resultCode, data);
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        bridge.onRequestPermissionsResult(requestCode, permissions, grantResults);
    }

    public String paseoCall(String method, String args) {
        return bridge.call(method, args);
    }

    public void paseoCallAsync(long id, String method, String args) {
        bridge.callAsync(id, method, args);
    }

    public void paseoAudioPlay(long request, byte[] pcm16, int channels, int sampleRate) {
        bridge.audio().play(request, pcm16, channels, sampleRate);
    }

    public void paseoAudioCancel() {
        bridge.audio().cancel();
    }
}
