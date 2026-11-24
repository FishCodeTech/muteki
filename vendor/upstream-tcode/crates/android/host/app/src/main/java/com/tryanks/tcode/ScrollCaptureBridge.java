package com.tryanks.tcode;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.PorterDuff;
import android.graphics.Rect;
import android.os.CancellationSignal;
import android.os.Handler;
import android.os.Looper;
import android.view.Choreographer;
import android.view.PixelCopy;
import android.view.ScrollCaptureCallback;
import android.view.ScrollCaptureSession;
import android.view.Surface;
import android.view.View;
import android.view.Window;
import androidx.annotation.RequiresApi;
import java.util.HashMap;
import java.util.Map;
import java.util.function.Consumer;

/**
 * Offers the GPUI window to the system screenshot tool's scrolling capture.
 *
 * <p>GPUI paints the whole window into one surface, so the framework finds no scrolling view
 * of its own. This invisible view covers the window and answers for the chat timeline: Rust
 * says which rectangle scrolls, scrolls it for each requested tile, and reports how far the
 * frame on screen is scrolled; the tile is then copied out of the window with {@link
 * PixelCopy}. All rectangles are in this view's coordinates, which are window coordinates
 * because the view fills the window.
 */
@RequiresApi(31)
final class ScrollCaptureBridge extends View implements ScrollCaptureCallback {
    /** The Rust side of the capture; every call is answered asynchronously through this view. */
    interface Host {
        void search(long request);
        void start();
        void image(long request, int top);
        void end();
    }

    private static final class Tile {
        final ScrollCaptureSession session;
        final Rect area;
        final Consumer<Rect> onComplete;

        Tile(ScrollCaptureSession session, Rect area, Consumer<Rect> onComplete) {
            this.session = session;
            this.area = area;
            this.onComplete = onComplete;
        }
    }

    private final Host host;
    private final Window window;
    private final Handler handler = new Handler(Looper.getMainLooper());
    private final Map<Long, Tile> tiles = new HashMap<>();
    private long nextRequest;
    private long searchRequest;
    private Consumer<Rect> searchReady;
    private final Rect scrollBounds = new Rect();

    ScrollCaptureBridge(Context context, Window window, Host host) {
        super(context);
        this.host = host;
        this.window = window;
        setWillNotDraw(true);
        setImportantForAccessibility(IMPORTANT_FOR_ACCESSIBILITY_NO);
        setScrollCaptureHint(SCROLL_CAPTURE_HINT_INCLUDE);
        setScrollCaptureCallback(this);
    }

    /**
     * The part of {@code area} a frame scrolled by {@code scrolled} shows, in the capture's
     * coordinates; empty once the content has run out in that direction.
     */
    static Rect visiblePart(Rect area, int scrolled, int viewportHeight) {
        Rect visible = new Rect(area);
        if (!visible.intersect(area.left, scrolled, area.right, scrolled + viewportHeight)) {
            return new Rect();
        }
        return visible;
    }

    @Override
    public void onScrollCaptureSearch(CancellationSignal signal, Consumer<Rect> onReady) {
        long request = ++nextRequest;
        searchRequest = request;
        searchReady = onReady;
        signal.setOnCancelListener(() -> {
            if (searchRequest == request) searchReady = null;
        });
        host.search(request);
    }

    /** Rust's answer to a search: the scrolling rectangle in window pixels, empty to decline. */
    void onBounds(long request, int left, int top, int right, int bottom) {
        if (request != searchRequest || searchReady == null) return;
        Consumer<Rect> ready = searchReady;
        searchReady = null;
        Rect bounds = new Rect(left, top, right, bottom);
        int[] location = new int[2];
        getLocationInWindow(location);
        bounds.offset(-location[0], -location[1]);
        ready.accept(bounds);
    }

    @Override
    public void onScrollCaptureStart(
            ScrollCaptureSession session, CancellationSignal signal, Runnable onReady) {
        scrollBounds.set(session.getScrollBounds());
        host.start();
        onReady.run();
    }

    @Override
    public void onScrollCaptureImageRequest(
            ScrollCaptureSession session,
            CancellationSignal signal,
            Rect captureArea,
            Consumer<Rect> onComplete) {
        long request = ++nextRequest;
        tiles.put(request, new Tile(session, new Rect(captureArea), onComplete));
        signal.setOnCancelListener(() -> tiles.remove(request));
        host.image(request, captureArea.top);
    }

    /** Rust's answer to a tile: the frame on screen is scrolled {@code scrolled} pixels from the start. */
    void onRendered(long request, boolean ok, int scrolled) {
        Tile tile = tiles.remove(request);
        if (tile == null) return;
        Rect visible = ok ? visiblePart(tile.area, scrolled, scrollBounds.height()) : new Rect();
        if (visible.isEmpty()) {
            tile.onComplete.accept(visible);
            return;
        }
        int[] location = new int[2];
        getLocationInWindow(location);
        Rect source = new Rect(visible);
        source.offset(location[0] + scrollBounds.left, location[1] + scrollBounds.top - scrolled);
        Bitmap bitmap = Bitmap.createBitmap(visible.width(), visible.height(), Bitmap.Config.ARGB_8888);
        // The frame was queued to the compositor; give it a vsync to become the window's content.
        Choreographer.getInstance().postFrameCallback(frameTime -> {
            try {
                PixelCopy.request(window, source, bitmap, result -> {
                    if (result == PixelCopy.SUCCESS && blit(tile, bitmap, visible)) {
                        tile.onComplete.accept(visible);
                    } else {
                        tile.onComplete.accept(new Rect());
                    }
                }, handler);
            } catch (IllegalArgumentException error) {
                tile.onComplete.accept(new Rect());
            }
        });
    }

    private static boolean blit(Tile tile, Bitmap bitmap, Rect visible) {
        Surface surface = tile.session.getSurface();
        if (!surface.isValid()) return false;
        Canvas canvas;
        try {
            canvas = surface.lockHardwareCanvas();
        } catch (IllegalArgumentException | Surface.OutOfResourcesException error) {
            return false;
        }
        try {
            canvas.drawColor(Color.TRANSPARENT, PorterDuff.Mode.CLEAR);
            canvas.drawBitmap(bitmap, visible.left - tile.area.left, visible.top - tile.area.top, null);
        } finally {
            surface.unlockCanvasAndPost(canvas);
        }
        return true;
    }

    @Override
    public void onScrollCaptureEnd(Runnable onReady) {
        for (Tile tile : tiles.values()) tile.onComplete.accept(new Rect());
        tiles.clear();
        host.end();
        onReady.run();
    }
}
