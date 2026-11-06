package sh.paseo.sessions;

import android.content.Context;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.media.AudioFormat;
import android.media.AudioManager;
import android.media.AudioTrack;
import android.os.Build;
import android.os.Handler;
import android.os.HandlerThread;

import java.util.ArrayDeque;

/**
 * Output-only PCM16 playback with transient-exclusive media audio focus
 * (original flutter_audio AudioEngine.kt). Sources play in submission order;
 * each completes with {@code true} when finished or {@code false} when
 * cancelled or interrupted. Works on every supported API level (AudioTrack).
 */
final class PaseoAudio {
    private static final class Source {
        final long request;
        final byte[] pcm;
        final int channels;
        final int rate;

        Source(long request, byte[] pcm, int channels, int rate) {
            this.request = request;
            this.pcm = pcm;
            this.channels = channels;
            this.rate = rate;
        }
    }

    private final AudioManager manager;
    private final HandlerThread thread = new HandlerThread("PaseoAudio");
    private final Handler handler;
    private final ArrayDeque<Source> queue = new ArrayDeque<>();
    private final AudioAttributes attributes = new AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
            .build();
    private AudioFocusRequest focus;
    private boolean focusHeld = false;
    private boolean playing = false;
    private volatile AudioTrack current;
    private volatile long generation = 0;

    PaseoAudio(Context context) {
        manager = (AudioManager) context.getSystemService(Context.AUDIO_SERVICE);
        thread.start();
        handler = new Handler(thread.getLooper());
    }

    private final AudioManager.OnAudioFocusChangeListener focusListener = change -> {
        if (change == AudioManager.AUDIOFOCUS_LOSS || change == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT
                || change == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK) {
            // The platform took the audio session away: nothing queued may resume.
            cancelInternal();
            abandonFocus();
            PaseoBridge.nativeEvent("audioInterrupted", "{}");
        }
    };

    @SuppressWarnings("deprecation")
    private boolean acquireFocus() {
        if (focusHeld) return true;
        int result;
        if (Build.VERSION.SDK_INT >= 26) {
            focus = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT_EXCLUSIVE)
                    .setAudioAttributes(attributes)
                    .setOnAudioFocusChangeListener(focusListener, handler)
                    .build();
            result = manager.requestAudioFocus(focus);
        } else {
            result = manager.requestAudioFocus(focusListener, AudioManager.STREAM_MUSIC, AudioManager.AUDIOFOCUS_GAIN_TRANSIENT_EXCLUSIVE);
        }
        focusHeld = result == AudioManager.AUDIOFOCUS_REQUEST_GRANTED;
        return focusHeld;
    }

    @SuppressWarnings("deprecation")
    private void abandonFocus() {
        if (!focusHeld) return;
        if (Build.VERSION.SDK_INT >= 26 && focus != null) manager.abandonAudioFocusRequest(focus);
        else manager.abandonAudioFocus(focusListener);
        focus = null;
        focusHeld = false;
    }

    void play(long request, byte[] pcm16, int channels, int rate) {
        handler.post(() -> {
            queue.add(new Source(request, pcm16, channels, rate));
            drain();
        });
    }

    void cancel() {
        handler.post(this::cancelInternal);
        AudioTrack track = current;
        generation++;
        if (track != null) {
            try {
                track.pause();
                track.flush();
            } catch (IllegalStateException ignored) {
            }
        }
    }

    private void cancelInternal() {
        generation++;
        while (!queue.isEmpty()) {
            PaseoBridge.nativeResult(queue.poll().request, PaseoBridge.ok(false));
        }
    }

    private void drain() {
        if (playing) return;
        Source next = queue.poll();
        if (next == null) {
            abandonFocus();
            return;
        }
        if (!acquireFocus()) {
            PaseoBridge.nativeResult(next.request, PaseoBridge.error("audio_focus", "Audio focus denied"));
            cancelInternal();
            return;
        }
        playing = true;
        long started = generation;
        boolean finished = playSource(next, started);
        playing = false;
        PaseoBridge.nativeResult(next.request, PaseoBridge.ok(finished && generation == started));
        handler.post(this::drain);
    }

    /** Blocking write of the whole source, then waits for the playback head. */
    private boolean playSource(Source source, long started) {
        int channelMask = source.channels >= 2 ? AudioFormat.CHANNEL_OUT_STEREO : AudioFormat.CHANNEL_OUT_MONO;
        int channels = source.channels >= 2 ? 2 : 1;
        AudioFormat format = new AudioFormat.Builder()
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .setSampleRate(source.rate)
                .setChannelMask(channelMask)
                .build();
        int minimum = AudioTrack.getMinBufferSize(source.rate, channelMask, AudioFormat.ENCODING_PCM_16BIT);
        AudioTrack track;
        try {
            track = new AudioTrack(attributes, format, Math.max(minimum, 8192), AudioTrack.MODE_STREAM, AudioManager.AUDIO_SESSION_ID_GENERATE);
        } catch (IllegalArgumentException error) {
            return false;
        }
        current = track;
        try {
            track.play();
            byte[] pcm = source.pcm;
            if (source.channels > 2) pcm = downmix(pcm, source.channels);
            int offset = 0;
            while (offset < pcm.length && generation == started) {
                int written = track.write(pcm, offset, Math.min(16384, pcm.length - offset));
                if (written < 0) return false;
                offset += written;
            }
            int frames = pcm.length / (2 * channels);
            while (generation == started && track.getPlaybackHeadPosition() < frames
                    && track.getPlayState() == AudioTrack.PLAYSTATE_PLAYING) {
                try {
                    Thread.sleep(10);
                } catch (InterruptedException e) {
                    return false;
                }
            }
            return generation == started;
        } finally {
            current = null;
            try {
                track.stop();
            } catch (IllegalStateException ignored) {
            }
            track.release();
        }
    }

    /** Keeps the first two channels of interleaved PCM16. */
    private static byte[] downmix(byte[] pcm, int channels) {
        int frames = pcm.length / (2 * channels);
        byte[] out = new byte[frames * 4];
        for (int frame = 0; frame < frames; frame++) {
            System.arraycopy(pcm, frame * channels * 2, out, frame * 4, 4);
        }
        return out;
    }

    void release() {
        cancel();
        handler.post(() -> {
            abandonFocus();
            thread.quitSafely();
        });
    }
}
