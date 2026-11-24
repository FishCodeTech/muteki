package com.tryanks.tcode;

import android.test.AndroidTestCase;
import android.text.Selection;
import android.text.SpannableStringBuilder;
import android.view.View;
import java.util.ArrayList;

/** Exercises the production connection against Android's real Editable/InputConnection code. */
@SuppressWarnings("deprecation")
public final class GpuiInputConnectionTest extends AndroidTestCase {
    private static final class FakeClipboard implements GpuiInputConnection.Clipboard {
        String text;
        @Override public String read() { return text; }
        @Override public void write(String value) { text = value; }
    }

    public void testImeEditPanelActionsEditTheDraftAndClipboard() {
        SpannableStringBuilder text = new SpannableStringBuilder("hello 世界");
        Selection.setSelection(text, 0);
        ArrayList<GpuiInputConnection.State> sent = new ArrayList<>();
        FakeClipboard clipboard = new FakeClipboard();
        GpuiInputConnection input =
                new GpuiInputConnection(new View(getContext()), text, clipboard, sent::add);
        assertFalse(input.performContextMenuAction(android.R.id.paste));
        assertFalse(input.performContextMenuAction(android.R.id.copy));
        assertTrue(sent.isEmpty());
        assertTrue(input.performContextMenuAction(android.R.id.selectAll));
        assertEquals(new GpuiInputConnection.State("hello 世界", 0, 8, -1, -1), sent.get(0));
        assertTrue(input.performContextMenuAction(android.R.id.copy));
        assertEquals("hello 世界", clipboard.text);
        assertEquals(1, sent.size());
        input.setSelection(6, 8);
        assertTrue(input.performContextMenuAction(android.R.id.cut));
        assertEquals("世界", clipboard.text);
        assertEquals(new GpuiInputConnection.State("hello ", 6, 6, -1, -1), sent.get(sent.size() - 1));
        clipboard.text = "again";
        assertTrue(input.performContextMenuAction(android.R.id.paste));
        assertEquals(new GpuiInputConnection.State("hello again", 11, 11, -1, -1), sent.get(sent.size() - 1));
    }

    public void testAutocompletePublishesTheCompleteReplacementOnce() {
        for (boolean composing : new boolean[] {false, true}) {
            SpannableStringBuilder text = new SpannableStringBuilder("an exmple here");
            Selection.setSelection(text, 9);
            ArrayList<GpuiInputConnection.State> sent = new ArrayList<>();
            GpuiInputConnection input = new GpuiInputConnection(new View(getContext()), text, new FakeClipboard(), sent::add);
            input.beginBatchEdit();
            if (composing) {
                input.setComposingRegion(3, 9);
            } else {
                input.deleteSurroundingText(6, 0);
            }
            input.commitText("example", 1);
            assertTrue(sent.isEmpty());
            input.endBatchEdit();
            assertEquals(1, sent.size());
            assertEquals(new GpuiInputConnection.State("an example here", 10, 10, -1, -1), sent.get(0));
        }
    }

    public void testSelectionAndUnicodeDeletionReachTheComposer() {
        SpannableStringBuilder text = new SpannableStringBuilder("😀word中");
        Selection.setSelection(text, text.length());
        ArrayList<GpuiInputConnection.State> sent = new ArrayList<>();
        GpuiInputConnection input = new GpuiInputConnection(new View(getContext()), text, new FakeClipboard(), sent::add);
        input.setSelection(2, 6);
        assertEquals(new GpuiInputConnection.State("😀word中", 2, 6, -1, -1), sent.get(0));
        input.commitText("example", 0);
        assertEquals(new GpuiInputConnection.State("😀example中", 2, 2, -1, -1), sent.get(1));
        input.deleteSurroundingTextInCodePoints(1, 0);
        assertEquals(new GpuiInputConnection.State("example中", 0, 0, -1, -1), sent.get(2));
        input.deleteSurroundingText(0, 7);
        assertEquals(new GpuiInputConnection.State("中", 0, 0, -1, -1), sent.get(3));
    }

    public void testRetiredConnectionCannotChangeTheNextDraft() {
        SpannableStringBuilder text = new SpannableStringBuilder("old");
        Selection.setSelection(text, text.length());
        ArrayList<GpuiInputConnection.State> sent = new ArrayList<>();
        GpuiInputConnection input = new GpuiInputConnection(new View(getContext()), text, new FakeClipboard(), sent::add);
        input.closeConnection();
        text.replace(0, text.length(), "new");
        input.commitText("stale suggestion", 1);
        assertEquals("new", text.toString());
        assertTrue(sent.isEmpty());
    }
}
