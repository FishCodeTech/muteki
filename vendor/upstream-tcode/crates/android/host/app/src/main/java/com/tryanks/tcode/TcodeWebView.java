package com.tryanks.tcode;

import android.app.Activity;
import android.content.Context;
import android.graphics.PixelFormat;
import android.os.Build;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.View;
import android.view.WindowManager;
import android.view.inputmethod.InputMethodManager;
import android.webkit.WebView;
import android.widget.FrameLayout;

/** A native child window is required: NativeActivity owns its main surface and
 * ViewRootImpl deliberately skips drawing ordinary addContentView children.
 * The child has its own input queue, so GPUI cannot consume page input first. */
final class TcodeWebView extends WebView {
    private final FrameLayout container;
    private final WindowManager manager;
    private final WindowManager.LayoutParams placement;
    private boolean attached;
    private boolean visible;
    private boolean paused;

    TcodeWebView(Activity activity) {
        super(activity);
        manager = activity.getWindowManager();
        container = new FrameLayout(activity);
        container.addView(this, new FrameLayout.LayoutParams(0, 0));
        placement = new WindowManager.LayoutParams(0, 0,
                WindowManager.LayoutParams.TYPE_APPLICATION_PANEL,
                WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL
                        | WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE
                        | WindowManager.LayoutParams.FLAG_WATCH_OUTSIDE_TOUCH
                        | WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN
                        | WindowManager.LayoutParams.FLAG_HARDWARE_ACCELERATED,
                PixelFormat.TRANSLUCENT);
        placement.token = activity.getWindow().getDecorView().getWindowToken();
        placement.gravity = Gravity.TOP | Gravity.LEFT;
        placement.softInputMode = WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE;
        placement.setTitle("tcode Preview");
        if (Build.VERSION.SDK_INT >= 28)
            placement.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
        if (Build.VERSION.SDK_INT >= 30) placement.setFitInsetsTypes(0);
        container.setOnTouchListener((view, event) -> {
            if (event.getActionMasked() == MotionEvent.ACTION_OUTSIDE) releasePreviewFocus();
            return false;
        });
    }

    void setPreviewBounds(int x, int y, int width, int height) {
        if (placement.x == x && placement.y == y && placement.width == width && placement.height == height) return;
        placement.x = x;
        placement.y = y;
        placement.width = width;
        placement.height = height;
        setLayoutParams(new FrameLayout.LayoutParams(width, height));
        if (attached) manager.updateViewLayout(container, placement);
    }

    void setPreviewVisible(boolean visible) {
        this.visible = visible;
        updateAttachment();
    }

    private void updateAttachment() {
        boolean show = visible && !paused && placement.width > 0 && placement.height > 0;
        if (show == attached) return;
        if (show) {
            manager.addView(container, placement);
            attached = true;
        } else {
            releasePreviewFocus();
            manager.removeViewImmediate(container);
            attached = false;
        }
    }

    void releasePreviewFocus() {
        if (hasFocus()) {
            ((InputMethodManager)getContext().getSystemService(Context.INPUT_METHOD_SERVICE))
                    .hideSoftInputFromWindow(getWindowToken(), 0);
            clearFocus();
        }
        if ((placement.flags & WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE) == 0) {
            placement.flags |= WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE;
            if (attached) manager.updateViewLayout(container, placement);
        }
    }

    @Override public boolean onTouchEvent(MotionEvent event) {
        if (event.getActionMasked() == MotionEvent.ACTION_DOWN) {
            placement.flags &= ~WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE;
            if (attached) manager.updateViewLayout(container, placement);
            requestFocus();
        }
        return super.onTouchEvent(event);
    }

    @Override public void onWindowFocusChanged(boolean focused) {
        super.onWindowFocusChanged(focused);
        // The first page tap changes window focus asynchronously. Retry the
        // WebView's own editor request once the new window actually owns IME.
        if (focused) post(() -> {
            if (hasWindowFocus() && onCheckIsTextEditor())
                ((InputMethodManager)getContext().getSystemService(Context.INPUT_METHOD_SERVICE))
                        .showSoftInput(this, InputMethodManager.SHOW_IMPLICIT);
        });
    }

    @Override public boolean dispatchKeyEvent(KeyEvent event) {
        if (super.dispatchKeyEvent(event)) return true;
        return event.getKeyCode() != KeyEvent.KEYCODE_BACK;
    }

    void pausePreview() { paused = true; updateAttachment(); onPause(); }
    void resumePreview() { paused = false; onResume(); updateAttachment(); }
    void destroyPreview() {
        setPreviewVisible(false);
        container.removeView(this);
        stopLoading();
        destroy();
    }
}
