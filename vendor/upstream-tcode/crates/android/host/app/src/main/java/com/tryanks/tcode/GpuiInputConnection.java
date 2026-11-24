package com.tryanks.tcode;

import android.text.Editable;
import android.text.Selection;
import android.view.View;
import android.view.inputmethod.BaseInputConnection;
import java.util.function.Consumer;

/** Publish Android's completed edits, including selection and composing-span changes. */
class GpuiInputConnection extends BaseInputConnection {
    record State(String text, int selectionStart, int selectionEnd, int composingStart, int composingEnd) {}
    interface Clipboard {
        String read();
        void write(String text);
    }
    private final Editable editable;
    private final Clipboard clipboard;
    private final Consumer<State> changed;
    private int batchDepth;
    private boolean closed;
    private State lastState;

    GpuiInputConnection(View view, Editable editable, Clipboard clipboard, Consumer<State> changed) {
        super(view, true);
        this.editable = editable;
        this.clipboard = clipboard;
        this.changed = changed;
        rememberState();
    }

    @Override public Editable getEditable() { return closed ? null : editable; }

    @Override public void closeConnection() {
        // restartInput retires the old connection after the app has supplied its new state.
        // BaseInputConnection.closeConnection would clear that new composing region.
        closed = true;
    }

    @Override public boolean beginBatchEdit() {
        batchDepth++;
        return true;
    }

    @Override public boolean endBatchEdit() {
        if (batchDepth == 0) return false;
        batchDepth--;
        publishChanges();
        return batchDepth > 0;
    }

    @Override public boolean setSelection(int start, int end) {
        boolean result = super.setSelection(start, end);
        publishChanges();
        return result;
    }

    // IME edit panels (select all, copy, cut, paste) arrive here; BaseInputConnection ignores them.
    @Override public boolean performContextMenuAction(int id) {
        if (closed) return false;
        if (id == android.R.id.selectAll) return setSelection(0, editable.length());
        if (id == android.R.id.paste) {
            String text = clipboard.read();
            return text != null && commitText(text, 1);
        }
        int start = Math.min(Selection.getSelectionStart(editable), Selection.getSelectionEnd(editable));
        int end = Math.max(Selection.getSelectionStart(editable), Selection.getSelectionEnd(editable));
        if ((id != android.R.id.copy && id != android.R.id.cut) || start < 0 || start == end) return false;
        clipboard.write(editable.subSequence(start, end).toString());
        if (id == android.R.id.cut) {
            beginBatchEdit();
            removeComposingSpans(editable);
            editable.delete(start, end);
            Selection.setSelection(editable, start);
            endBatchEdit();
        }
        return true;
    }

    void publishChanges() {
        if (closed || batchDepth != 0) return;
        State state = readState();
        if (!state.equals(lastState)) {
            lastState = state;
            changed.accept(state);
        }
    }

    void rememberState() { lastState = readState(); }

    private State readState() {
        return new State(editable.toString(), Selection.getSelectionStart(editable),
                Selection.getSelectionEnd(editable), getComposingSpanStart(editable),
                getComposingSpanEnd(editable));
    }
}
