package com.tryanks.tcode;

import android.graphics.Rect;
import android.test.AndroidTestCase;

/** The tile geometry the bridge derives from Rust's reported scroll position. */
@SuppressWarnings("deprecation")
public final class ScrollCaptureBridgeTest extends AndroidTestCase {
    public void testVisiblePartClipsTheTileToTheFrameOnScreen() {
        // The frame reached the requested tile in full.
        assertEquals(new Rect(0, -800, 400, 0),
                ScrollCaptureBridge.visiblePart(new Rect(0, -800, 400, 0), -800, 800));
        // The content ran out 300 pixels above the start: only the bottom of the tile exists.
        assertEquals(new Rect(0, -300, 400, 0),
                ScrollCaptureBridge.visiblePart(new Rect(0, -800, 400, 0), -300, 800));
        // Past the end of the content the tile is empty, which ends the capture.
        assertTrue(ScrollCaptureBridge.visiblePart(new Rect(0, 1600, 400, 2400), 700, 800).isEmpty());
        // A short last tile at the end of the content.
        assertEquals(new Rect(0, 800, 400, 1100),
                ScrollCaptureBridge.visiblePart(new Rect(0, 800, 400, 1600), 300, 800));
    }
}
