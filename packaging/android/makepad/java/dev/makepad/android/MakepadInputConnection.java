package dev.makepad.android;

import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.os.Handler;
import android.text.Editable;
import android.text.InputType;
import android.text.Selection;
import android.text.SpannableStringBuilder;
import android.view.KeyEvent;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.ExtractedText;
import android.view.inputmethod.ExtractedTextRequest;
import android.view.inputmethod.InputMethodManager;
import java.util.function.BooleanSupplier;

/** Paseo: a full editor, not BaseInputConnection's key-event fallback mode.
 * Android owns UTF-16 selection/composing spans; Rust owns the widget and undo.
 * Session + revision fence delayed callbacks and acknowledgements across focus.
 */
final class MakepadInputConnection extends BaseInputConnection {
    private final MakepadSurface view;
    private long session;
    private final Editable editable;
    private long revision;
    private int batchDepth;
    private boolean closed;
    private boolean dirty;
    private String sentText;
    private int sentStart, sentEnd, sentComposeStart = -1, sentComposeEnd = -1;
    private ExtractedTextRequest extractedRequest;

    MakepadInputConnection(MakepadSurface view, long session, String text, int start, int end) {
        super(view, true);
        this.view = view;
        this.session = session;
        editable = new SpannableStringBuilder(text);
        Selection.setSelection(editable, start, end);
        sentText = text;
        sentStart = start;
        sentEnd = end;
    }

    MakepadInputConnection reconnect() {
        publish();
        // A framework restart closes the old connection. Never hand it out again.
        MakepadInputConnection next = new MakepadInputConnection(view, session,
            editable.toString(), Selection.getSelectionStart(editable), Selection.getSelectionEnd(editable));
        next.revision = revision;
        int start = getComposingSpanStart(editable), end = getComposingSpanEnd(editable);
        if (!closed && start >= 0 && end >= start) next.restoreComposition(start, end);
        invalidate();
        return next;
    }

    private void restoreComposition(int start, int end) {
        super.setComposingRegion(start, end);
        sentComposeStart = start;
        sentComposeEnd = end;
    }

    void flushBeforeNativeAction() {
        if (closed) return;
        finishComposingText();
        publish();
    }

    void editorInfo(EditorInfo info, boolean password, boolean numeric, boolean terminal) {
        // Terminal editors keep shell input literal: no auto-capitalization, suggestion strip or
        // autocorrect; composing input methods (Hangul) stay available.
        info.inputType = numeric
            ? InputType.TYPE_CLASS_NUMBER | (password ? InputType.TYPE_NUMBER_VARIATION_PASSWORD
                : InputType.TYPE_NUMBER_FLAG_DECIMAL)
            : InputType.TYPE_CLASS_TEXT | (password ? InputType.TYPE_TEXT_VARIATION_PASSWORD
                : terminal ? InputType.TYPE_TEXT_FLAG_MULTI_LINE | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
                : InputType.TYPE_TEXT_FLAG_MULTI_LINE | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
        info.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN | EditorInfo.IME_FLAG_NO_EXTRACT_UI
            | EditorInfo.IME_ACTION_NONE;
        info.initialSelStart = Selection.getSelectionStart(editable);
        info.initialSelEnd = Selection.getSelectionEnd(editable);
        if (android.os.Build.VERSION.SDK_INT >= 30) info.setInitialSurroundingText(editable);
    }

    @Override public Handler getHandler() { return view.getHandler(); }
    @Override public Editable getEditable() { return editable; }
    @Override public boolean beginBatchEdit() {
        if (closed) return false;
        batchDepth++;
        return true;
    }
    @Override public boolean endBatchEdit() {
        if (closed || batchDepth == 0) return false;
        batchDepth--;
        if (batchDepth == 0) publish();
        return batchDepth > 0;
    }
    private boolean edit(BooleanSupplier operation) {
        if (!beginBatchEdit()) return false;
        try {
            boolean result = operation.getAsBoolean();
            dirty |= result;
            return result;
        } finally { endBatchEdit(); }
    }
    @Override public boolean setComposingText(CharSequence text, int cursor) {
        return edit(() -> super.setComposingText(text, cursor));
    }
    @Override public boolean setComposingRegion(int start, int end) {
        return edit(() -> super.setComposingRegion(start, end));
    }
    @Override public boolean commitText(CharSequence text, int cursor) {
        return edit(() -> super.commitText(text, cursor));
    }
    @Override public boolean finishComposingText() {
        return edit(() -> super.finishComposingText());
    }
    @Override public boolean setSelection(int start, int end) {
        return edit(() -> super.setSelection(start, end));
    }
    @Override public boolean deleteSurroundingText(int before, int after) {
        return edit(() -> super.deleteSurroundingText(before, after));
    }
    @Override public boolean deleteSurroundingTextInCodePoints(int before, int after) {
        return edit(() -> super.deleteSurroundingTextInCodePoints(before, after));
    }
    @Override public void closeConnection() {
        if (closed) return;
        finishComposingText();
        // An interrupted batch must not strand already accepted characters.
        batchDepth = 0;
        publish();
        closed = true;
    }
    void invalidate() { closed = true; }

    private InputMethodManager manager() {
        return (InputMethodManager) view.getContext().getSystemService(Context.INPUT_METHOD_SERVICE);
    }
    private void publish() {
        if (closed || !dirty) return;
        dirty = false;
        String text = editable.toString();
        int start = Selection.getSelectionStart(editable), end = Selection.getSelectionEnd(editable);
        int composeStart = getComposingSpanStart(editable), composeEnd = getComposingSpanEnd(editable);
        if (text.equals(sentText) && start == sentStart && end == sentEnd
                && composeStart == sentComposeStart && composeEnd == sentComposeEnd) return;
        sentText = text;
        sentStart = start;
        sentEnd = end;
        sentComposeStart = composeStart;
        sentComposeEnd = composeEnd;
        MakepadNative.surfaceOnTextEdit(session, ++revision, text, start, end, composeStart, composeEnd);
        manager().updateSelection(view, start, end, composeStart, composeEnd);
        if (extractedRequest != null) manager().updateExtractedText(view,
            extractedRequest.token, extractedText());
    }

    // Native cursor moves fence old callbacks without restarting the IME.
    void synchronize(long session, String text, int start, int end) {
        this.session = session;
        revision = 0;
        batchDepth = 0;
        dirty = false;
        removeComposingSpans(editable);
        boolean textChanged = !text.contentEquals(editable);
        if (textChanged) editable.replace(0, editable.length(), text);
        Selection.setSelection(editable, start, end);
        sentText = text;
        sentStart = start;
        sentEnd = end;
        sentComposeStart = sentComposeEnd = -1;
        notifyExternalChange(textChanged, start, end);
    }

    void acknowledge(long acknowledgedRevision, String text, int start, int end) {
        // Never replace a newer local composition with a render-thread echo.
        if (closed || acknowledgedRevision != revision) return;
        // An open batch can already contain a newer edit without a new revision.
        // A no-op batch still permits the native editor's correction.
        if (dirty && (!sentText.contentEquals(editable)
                || sentStart != Selection.getSelectionStart(editable)
                || sentEnd != Selection.getSelectionEnd(editable)
                || sentComposeStart != getComposingSpanStart(editable)
                || sentComposeEnd != getComposingSpanEnd(editable))) return;
        boolean textChanged = !text.contentEquals(editable);
        if (textChanged || start != Selection.getSelectionStart(editable)
                || end != Selection.getSelectionEnd(editable)) {
            if (textChanged) editable.replace(0, editable.length(), text);
            removeComposingSpans(editable);
            Selection.setSelection(editable, start, end);
            sentText = text;
            sentStart = start;
            sentEnd = end;
            sentComposeStart = sentComposeEnd = -1;
            notifyExternalChange(textChanged, start, end);
        }
    }

    private void notifyExternalChange(boolean textChanged, int start, int end) {
        manager().updateSelection(view, start, end, -1, -1);
        if (extractedRequest != null) manager().updateExtractedText(view,
            extractedRequest.token, extractedText());
        if (textChanged) {
            if (android.os.Build.VERSION.SDK_INT >= 33) manager().invalidateInput(view);
            else manager().restartInput(view);
        }
    }

    private ExtractedText extractedText() {
        ExtractedText result = new ExtractedText();
        result.text = editable.toString();
        result.startOffset = 0;
        result.partialStartOffset = result.partialEndOffset = -1;
        result.selectionStart = Selection.getSelectionStart(editable);
        result.selectionEnd = Selection.getSelectionEnd(editable);
        return result;
    }
    @Override public ExtractedText getExtractedText(ExtractedTextRequest request, int flags) {
        if (closed) return null;
        if ((flags & GET_EXTRACTED_TEXT_MONITOR) != 0) extractedRequest = request;
        return extractedText();
    }
    @Override public boolean performContextMenuAction(int id) {
        if (closed) return false;
        if (id == android.R.id.selectAll) return setSelection(0, editable.length());
        ClipboardManager clipboard = (ClipboardManager) view.getContext()
            .getSystemService(Context.CLIPBOARD_SERVICE);
        int start = Math.min(Selection.getSelectionStart(editable), Selection.getSelectionEnd(editable));
        int end = Math.max(Selection.getSelectionStart(editable), Selection.getSelectionEnd(editable));
        if (id == android.R.id.copy || id == android.R.id.cut) {
            clipboard.setPrimaryClip(ClipData.newPlainText("Paseo", editable.subSequence(start, end)));
            if (id == android.R.id.cut) return edit(() -> {
                removeComposingSpans(editable);
                editable.delete(start, end);
                Selection.setSelection(editable, start);
                return true;
            });
            return true;
        }
        if (id == android.R.id.paste || id == android.R.id.pasteAsPlainText) {
            ClipData clip = clipboard.getPrimaryClip();
            return clip != null && clip.getItemCount() > 0
                && commitText(clip.getItemAt(0).coerceToText(view.getContext()), 1);
        }
        return false;
    }
    @Override public boolean sendKeyEvent(KeyEvent event) {
        if (closed) return false;
        return view.onKey(view, event.getKeyCode(), event);
    }
    // Hardware text and IME-generated delete events update the same shadow so
    // subsequent surrounding-text requests never observe stale native text.
    boolean handleTextKey(KeyEvent event) {
        if (closed) return false;
        int key = event.getKeyCode();
        if (event.isCtrlPressed() && !event.isAltPressed() && !event.isMetaPressed()) {
            int action;
            switch (key) {
                case KeyEvent.KEYCODE_A: action = android.R.id.selectAll; break;
                case KeyEvent.KEYCODE_C: action = android.R.id.copy; break;
                case KeyEvent.KEYCODE_X: action = android.R.id.cut; break;
                case KeyEvent.KEYCODE_V: action = android.R.id.paste; break;
                default: return false;
            }
            if (event.getAction() == KeyEvent.ACTION_DOWN) {
                finishComposingText();
                performContextMenuAction(action);
            }
            return true;
        }
        if (event.isCtrlPressed() || event.isAltPressed() || event.isMetaPressed()) return false;
        int character = event.getUnicodeChar();
        boolean delete = key == KeyEvent.KEYCODE_DEL || key == KeyEvent.KEYCODE_FORWARD_DEL;
        String multiple = event.getAction() == KeyEvent.ACTION_MULTIPLE ? event.getCharacters() : null;
        if (!delete && (character < 32 || character == 127) && multiple == null) return false;
        if (event.getAction() == KeyEvent.ACTION_UP) return true;
        if (delete) return edit(() -> {
            removeComposingSpans(editable);
            int start = Math.min(Selection.getSelectionStart(editable), Selection.getSelectionEnd(editable));
            int end = Math.max(Selection.getSelectionStart(editable), Selection.getSelectionEnd(editable));
            if (start != end) {
                editable.delete(start, end);
                Selection.setSelection(editable, start);
                return true;
            }
            return super.deleteSurroundingTextInCodePoints(key == KeyEvent.KEYCODE_DEL ? 1 : 0,
                key == KeyEvent.KEYCODE_FORWARD_DEL ? 1 : 0);
        });
        if (multiple != null) return commitText(multiple, 1);
        if (!Character.isValidCodePoint(character)) return false;
        return edit(() -> {
            // A hardware character inserts at the caret; it is not replacement
            // text for the IME's current composing word.
            removeComposingSpans(editable);
            return super.commitText(new String(Character.toChars(character)), 1);
        });
    }
}
