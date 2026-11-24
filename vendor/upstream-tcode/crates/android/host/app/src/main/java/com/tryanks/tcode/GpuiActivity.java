package com.tryanks.tcode;

import android.app.NativeActivity;
import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ActivityInfo;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.graphics.Color;
import android.hardware.display.DisplayManager;
import android.os.Handler;
import android.os.Looper;
import android.view.Choreographer;
import android.view.Display;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;
import android.text.Editable;
import android.text.InputType;
import android.text.Selection;
import android.text.SpannableStringBuilder;
import android.text.method.TextKeyListener;
import android.util.Log;
import android.view.Gravity;
import android.view.HapticFeedbackConstants;
import android.view.KeyEvent;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.FrameLayout;
import java.util.Locale;

/** Minimal NativeActivity host for GPUI. */
public final class GpuiActivity extends NativeActivity {
    private boolean framePosted;
    private boolean framesActive;
    private Surface renderSurface;
    private float maximumFrameRate;
    private DisplayManager displayManager;
    private final Choreographer.FrameCallback frameCallback = frameTimeNanos -> {
        framePosted = false;
        if (renderSurface != null && renderSurface.isValid()) nativeFrame();
    };
    private final DisplayManager.DisplayListener displayListener = new DisplayManager.DisplayListener() {
        @Override public void onDisplayAdded(int id) { updateDisplayRate(); }
        @Override public void onDisplayRemoved(int id) { updateDisplayRate(); }
        @Override public void onDisplayChanged(int id) { updateDisplayRate(); }
    };

    private native void nativeFrame();
    private native void nativeRefreshRate(float rate);

    private void updateDisplayRate() {
        Display display = getWindow().getDecorView().getDisplay();
        if (display == null) return;
        Display.Mode current = display.getMode();
        maximumFrameRate = current.getRefreshRate();
        for (Display.Mode mode : display.getSupportedModes()) {
            if (mode.getPhysicalWidth() == current.getPhysicalWidth()
                    && mode.getPhysicalHeight() == current.getPhysicalHeight()) {
                maximumFrameRate = Math.max(maximumFrameRate, mode.getRefreshRate());
            }
        }
        nativeRefreshRate(maximumFrameRate);
        if (framesActive) hintFrameRate(maximumFrameRate);
    }

    private void hintFrameRate(float rate) {
        if (renderSurface == null || !renderSurface.isValid()) return;
        if (Build.VERSION.SDK_INT >= 31) {
            renderSurface.setFrameRate(rate, Surface.FRAME_RATE_COMPATIBILITY_DEFAULT,
                    Surface.CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS);
        } else if (Build.VERSION.SDK_INT >= 30) {
            renderSurface.setFrameRate(rate, Surface.FRAME_RATE_COMPATIBILITY_DEFAULT);
        }
    }

    public void gpuiRequestFrame() {
        if (renderSurface == null || !renderSurface.isValid()) return;
        if (!framesActive) {
            framesActive = true;
            hintFrameRate(maximumFrameRate);
        }
        if (!framePosted) {
            framePosted = true;
            Choreographer.getInstance().postFrameCallback(frameCallback);
        }
    }

    public void gpuiPauseFrames() {
        if (framePosted) Choreographer.getInstance().removeFrameCallback(frameCallback);
        framePosted = false;
        if (framesActive) {
            framesActive = false;
            // Zero releases the vote while the last buffer remains visible.
            hintFrameRate(0);
        }
    }

    @Override public void surfaceCreated(SurfaceHolder holder) {
        renderSurface = holder.getSurface();
        updateDisplayRate();
        super.surfaceCreated(holder);
        gpuiRequestFrame();
    }

    @Override public void surfaceDestroyed(SurfaceHolder holder) {
        gpuiPauseFrames();
        renderSurface = null;
        super.surfaceDestroyed(holder);
    }

    private static final int REQUEST_CAMERA = 6102;
    private static final int REQUEST_IMAGES = 6103;
    private static final int HOST_OK = 0;
    private static final int HOST_CANCELLED = 1;
    private static final int HOST_ERROR = 2;

    public PreviewHost previewHost;
    private GpuiInputView inputView;
    /** Android 12 and later only; null below, where scrolling capture does not exist. */
    private ScrollCaptureBridge scrollCapture;
    private Boolean appBackgroundDark;
    private boolean keyboardVisible;
    private boolean keyboardShowPending;
    private long cameraRequest;
    private long imageRequest;
    private int imageLimit;
    private ConnectivityManager.NetworkCallback networkCallback;
    private android.net.wifi.WifiManager.MulticastLock multicastLock;

    /**
     * Tells the Traverse endpoint to rebind and probe when the default network
     * changes. {@code onCapabilitiesChanged} also fires for signal-strength
     * updates, so only a change of transport or validation counts; the
     * callback runs on the connectivity thread and the native side forwards
     * to the GPUI thread.
     */
    private void watchDefaultNetwork() {
        ConnectivityManager connectivity = (ConnectivityManager)
            getApplicationContext().getSystemService(Context.CONNECTIVITY_SERVICE);
        if (connectivity == null) return;
        networkCallback = new ConnectivityManager.NetworkCallback() {
            private Network current;
            private int transports = -1;
            private boolean validated;

            @Override public void onAvailable(Network network) {
                current = network;
                transports = -1;
                nativeNetworkChanged();
            }

            @Override public void onLost(Network network) {
                if (network.equals(current)) current = null;
                nativeNetworkChanged();
            }

            @Override public void onCapabilitiesChanged(Network network, NetworkCapabilities capabilities) {
                int bits = transportBits(capabilities);
                boolean valid = capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED);
                if (bits == transports && valid == validated) return;
                boolean first = transports == -1;
                transports = bits;
                validated = valid;
                // onAvailable already reported the network; its first
                // capabilities are not a second change.
                if (!first) nativeNetworkChanged();
            }
        };
        connectivity.registerDefaultNetworkCallback(networkCallback);
    }

    private static int transportBits(NetworkCapabilities capabilities) {
        int bits = 0;
        for (int transport = 0; transport < 32; transport++) {
            if (capabilities.hasTransport(transport)) bits |= 1 << transport;
        }
        return bits;
    }

    private void unwatchDefaultNetwork() {
        if (networkCallback == null) return;
        ConnectivityManager connectivity = (ConnectivityManager)
            getApplicationContext().getSystemService(Context.CONNECTIVITY_SERVICE);
        if (connectivity != null) connectivity.unregisterNetworkCallback(networkCallback);
        networkCallback = null;
    }

    /**
     * Held by the Traverse LAN lookup around each DNS-SD browse, from its own
     * threads; reference counting allows overlapping browses.
     */
    public synchronized void gpuiMulticastLock(boolean acquire) {
        if (acquire) {
            if (multicastLock == null) {
                android.net.wifi.WifiManager wifi = (android.net.wifi.WifiManager)
                    getApplicationContext().getSystemService(Context.WIFI_SERVICE);
                if (wifi == null) return;
                multicastLock = wifi.createMulticastLock("tcode-lan-lookup");
                multicastLock.setReferenceCounted(true);
            }
            multicastLock.acquire();
        } else if (multicastLock != null && multicastLock.isHeld()) {
            multicastLock.release();
        }
    }


    private native void nativeCommitText(String text);
    private native void nativeSetComposingText(String text);
    private native void nativeFinishComposingText();
    private native void nativeDeleteBackward();
    private native void nativeInputState(long revision, long serial, String text,
            int selectionStart, int selectionEnd, int composingStart, int composingEnd);
    private native void nativeKeyEvent(int keyCode, boolean down, int unicodeCodePoint, int metaState);
    private native void nativeOnInsets(int left, int top, int right, int bottom, int imeBottom);
    private native void nativeOnBack(boolean enabled);
    private native void nativeQrScanCompleted(long requestId, int status, String value);
    private native void nativeImagePicked(long requestId, String name, String mime, byte[] bytes);
    private native void nativeImagePickFinished(long requestId, int status, String error);
    private native void nativeNetworkChanged();
    private native void nativeScrollCaptureSearch(long request);
    private native void nativeScrollCaptureStart();
    private native void nativeScrollCaptureImage(long request, int top);
    private native void nativeScrollCaptureEnd();

    private native boolean nativeFirstFrameRendered();

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        androidx.core.splashscreen.SplashScreen splash =
                androidx.core.splashscreen.SplashScreen.installSplashScreen(this);
        ensureNativeLibraryVisibleToJvm();
        super.onCreate(savedInstanceState);
        splash.setKeepOnScreenCondition(() -> !nativeFirstFrameRendered());
        configureEdgeToEdgeWindow();

        inputView = new GpuiInputView(this);
        inputView.setFocusable(false);
        inputView.setFocusableInTouchMode(false);
        inputView.setAlpha(0.01f);
        FrameLayout.LayoutParams layout = new FrameLayout.LayoutParams(1, 1);
        layout.gravity = Gravity.BOTTOM | Gravity.START;
        addContentView(inputView, layout);
        previewHost = new PreviewHost(this);
        if (Build.VERSION.SDK_INT >= 31) {
            scrollCapture = new ScrollCaptureBridge(this, getWindow(), new ScrollCaptureBridge.Host() {
                @Override public void search(long request) { nativeScrollCaptureSearch(request); }
                @Override public void start() { nativeScrollCaptureStart(); }
                @Override public void image(long request, int top) { nativeScrollCaptureImage(request, top); }
                @Override public void end() { nativeScrollCaptureEnd(); }
            });
            addContentView(scrollCapture, new FrameLayout.LayoutParams(
                    FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT));
        }

        View decor = getWindow().getDecorView();
        decor.setOnApplyWindowInsetsListener((view, insets) -> {
            publishInsets(insets);
            return insets;
        });
        decor.requestApplyInsets();
        displayManager = (DisplayManager) getSystemService(Context.DISPLAY_SERVICE);
        displayManager.registerDisplayListener(displayListener, new Handler(Looper.getMainLooper()));
        updateDisplayRate();
        watchDefaultNetwork();
    }

    @Override
    protected void onDestroy() {
        if (displayManager != null) displayManager.unregisterDisplayListener(displayListener);
        gpuiPauseFrames();
        unwatchDefaultNetwork();
        super.onDestroy();
    }

    @Override
    protected void onResume() {
        super.onResume();
        // The network may have changed while the activity was stopped without
        // the callback being delivered; the endpoint probes on every return.
        nativeNetworkChanged();
        View decor = getWindow().getDecorView();
        decor.post(() -> {
            WindowInsets insets = decor.getRootWindowInsets();
            if (insets != null) publishInsets(insets);
        });
    }

    private void ensureNativeLibraryVisibleToJvm() {
        try {
            ActivityInfo info = getPackageManager().getActivityInfo(
                    new ComponentName(this, getClass()), PackageManager.GET_META_DATA);
            String library = info.metaData == null
                    ? null : info.metaData.getString("android.app.lib_name");
            if (library == null || library.isEmpty()) {
                throw new IllegalStateException("android.app.lib_name is required");
            }
            System.loadLibrary(library);
        } catch (PackageManager.NameNotFoundException error) {
            throw new IllegalStateException("Unable to resolve GPUI activity metadata", error);
        }
    }

    @SuppressWarnings("deprecation")
    private void configureEdgeToEdgeWindow() {
        Window window = getWindow();
        boolean lightAppearance = appBackgroundDark != null ? !appBackgroundDark
                : (getResources().getConfiguration().uiMode
                    & Configuration.UI_MODE_NIGHT_MASK) != Configuration.UI_MODE_NIGHT_YES;
        window.setStatusBarColor(Color.TRANSPARENT);
        window.setNavigationBarColor(Color.TRANSPARENT);
        if (Build.VERSION.SDK_INT >= 29) {
            window.setStatusBarContrastEnforced(false);
            window.setNavigationBarContrastEnforced(false);
        }
        if (Build.VERSION.SDK_INT >= 30) {
            window.setDecorFitsSystemWindows(false);
            WindowInsetsController controller = window.getInsetsController();
            if (controller != null) {
                int lightBars = WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                        | WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS;
                controller.setSystemBarsAppearance(lightAppearance ? lightBars : 0, lightBars);
            }
        } else {
            int visibility =
                    View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION;
            if (lightAppearance) {
                visibility |= View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR
                        | View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR;
            }
            window.getDecorView().setSystemUiVisibility(visibility);
        }
    }

    /** GPUI resolves app preferences independently of Android's system appearance. */
    public void gpuiSetAppBackgroundDark(boolean dark) {
        appBackgroundDark = dark;
        Configuration configuration = new Configuration(getResources().getConfiguration());
        configuration.uiMode = (configuration.uiMode & ~Configuration.UI_MODE_NIGHT_MASK)
                | (dark ? Configuration.UI_MODE_NIGHT_YES : Configuration.UI_MODE_NIGHT_NO);
        int canvas = createConfigurationContext(configuration).getColor(R.color.canvas);
        getWindow().setBackgroundDrawable(new android.graphics.drawable.ColorDrawable(canvas));
        configureEdgeToEdgeWindow();
    }

    @SuppressWarnings("deprecation")
    private void publishInsets(WindowInsets insets) {
        boolean visible = Build.VERSION.SDK_INT >= 30
                ? insets.isVisible(WindowInsets.Type.ime())
                : insets.getSystemWindowInsetBottom() > insets.getStableInsetBottom();
        if (keyboardVisible && !visible) {
            keyboardShowPending = false;
            releaseInputFocus();
        }
        keyboardVisible = visible;
        if (Build.VERSION.SDK_INT >= 30) {
            android.graphics.Insets safe = insets.getInsets(
                    WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
            android.graphics.Insets ime = insets.getInsets(WindowInsets.Type.ime());
            nativeOnInsets(safe.left, safe.top, safe.right, safe.bottom, ime.bottom);
        } else {
            nativeOnInsets(insets.getStableInsetLeft(), insets.getStableInsetTop(),
                    insets.getStableInsetRight(), insets.getStableInsetBottom(),
                    insets.getSystemWindowInsetBottom());
        }
    }

    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() { nativeOnBack(true); }

    public void gpuiHapticLongPress() {
        getWindow().getDecorView().performHapticFeedback(HapticFeedbackConstants.LONG_PRESS);
    }

    public void gpuiHapticThreshold() {
        getWindow().getDecorView().performHapticFeedback(Build.VERSION.SDK_INT >= 34
                ? HapticFeedbackConstants.GESTURE_THRESHOLD_ACTIVATE
                : HapticFeedbackConstants.CONTEXT_CLICK);
    }

    public void gpuiHapticSelection() {
        getWindow().getDecorView().performHapticFeedback(Build.VERSION.SDK_INT >= 34
                ? HapticFeedbackConstants.SEGMENT_TICK
                : HapticFeedbackConstants.CLOCK_TICK);
    }

    public void gpuiShowKeyboard() {
        keyboardShowPending = true;
        inputView.setFocusable(true);
        inputView.setFocusableInTouchMode(true);
        inputView.requestFocus();
        inputView.post(this::showKeyboardIfReady);
    }

    private void showKeyboardIfReady() {
        if (!keyboardShowPending || !hasWindowFocus()) return;
        keyboardShowPending = !((InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE))
                .showSoftInput(inputView, InputMethodManager.SHOW_IMPLICIT);
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus && inputView != null && keyboardShowPending) {
            inputView.post(this::showKeyboardIfReady);
        }
    }

    public void gpuiHideKeyboard() {
        keyboardShowPending = false;
        InputMethodManager manager =
                (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
        manager.hideSoftInputFromWindow(inputView.getWindowToken(), 0);
        releaseInputFocus();
    }

    private void releaseInputFocus() {
        inputView.clearFocus();
        inputView.setFocusable(false);
        inputView.setFocusableInTouchMode(false);
    }

    public void gpuiConfigureInput(
            boolean autocorrect, int autocapitalize, boolean suggestions, int inputAction, boolean multiLine) {
        inputView.configure(autocorrect, autocapitalize, suggestions, inputAction, multiLine);
        ((InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE)).restartInput(inputView);
    }

    public void gpuiSyncInput(long revision, long serial, String text,
            int selectionStart, int selectionEnd, int composingStart, int composingEnd) {
        inputView.syncInput(revision, serial, text, selectionStart, selectionEnd,
                composingStart, composingEnd);
    }

    public int[] gpuiRasterizeEmoji(int glyph, float size) throws java.io.IOException {
        return SystemEmoji.rasterize(glyph, size);
    }

    public void gpuiFinish() { finish(); }

    public void gpuiOpenUrl(String url) {
        try {
            startActivity(new Intent(Intent.ACTION_VIEW, Uri.parse(url)));
        } catch (ActivityNotFoundException error) {
            Log.w("Tcode", "No application can open this URL", error);
        }
    }

    public String gpuiDataDir() {
        return getFilesDir().getAbsolutePath();
    }

    /** The user-facing device name: the Settings name, else the marketing name, else make and model. */
    public String gpuiDeviceModel() {
        String name = Settings.Global.getString(getContentResolver(), "device_name");
        if (name != null && !name.trim().isEmpty()) return name.trim();
        name = marketName();
        if (name != null && !name.trim().isEmpty()) return name.trim();
        String manufacturer = Build.MANUFACTURER == null ? "" : Build.MANUFACTURER.trim();
        String model = Build.MODEL == null ? "" : Build.MODEL.trim();
        if (model.isEmpty()) return manufacturer.isEmpty() ? "Android" : manufacturer;
        if (manufacturer.isEmpty() || model.toLowerCase(Locale.ROOT).startsWith(manufacturer.toLowerCase(Locale.ROOT))) {
            return model;
        }
        return manufacturer + " " + model;
    }

    public String gpuiDevicePlatform() {
        String release = Build.VERSION.RELEASE == null ? "" : Build.VERSION.RELEASE.trim();
        return release.isEmpty() ? "Android" : "Android " + release;
    }

    /** {@code ro.product.marketname} is a vendor property with no public API. */
    private static String marketName() {
        try {
            Class<?> properties = Class.forName("android.os.SystemProperties");
            Object value = properties.getMethod("get", String.class).invoke(null, "ro.product.marketname");
            return value instanceof String ? (String) value : null;
        } catch (Exception | LinkageError error) {
            return null;
        }
    }

    /** Where {@code text}'s words begin and end, in UTF-16 units, for a double-tapped word. */
    public int[] gpuiWordBreaks(String text) {
        android.icu.text.BreakIterator iterator = android.icu.text.BreakIterator.getWordInstance();
        iterator.setText(text);
        int[] breaks = new int[text.length() + 1];
        int count = 0;
        for (int boundary = iterator.first();
                boundary != android.icu.text.BreakIterator.DONE && count < breaks.length;
                boundary = iterator.next()) {
            breaks[count++] = boundary;
        }
        return java.util.Arrays.copyOf(breaks, count);
    }

    /** Snapshot used by the Rust shell at startup; restart after changing the OS language. */
    @SuppressWarnings("deprecation")
    public String gpuiSystemLocale() {
        Configuration configuration = getResources().getConfiguration();
        Locale locale = null;
        if (Build.VERSION.SDK_INT >= 24 && !configuration.getLocales().isEmpty()) {
            locale = configuration.getLocales().get(0);
        } else {
            locale = configuration.locale;
        }
        if (locale == null) locale = Locale.getDefault();
        return locale.toLanguageTag();
    }

    public void gpuiStartCameraScan(long requestId) {
        if (cameraRequest != 0) {
            nativeQrScanCompleted(requestId, HOST_ERROR, "相机扫描正在进行");
            return;
        }
        cameraRequest = requestId;
        try {
            startActivityForResult(new Intent(this, QrScannerActivity.class), REQUEST_CAMERA);
        } catch (RuntimeException error) {
            cameraRequest = 0;
            nativeQrScanCompleted(requestId, HOST_ERROR, errorMessage(error));
        }
    }

    /**
     * The system photo picker (Android 13+, no permission) or the document
     * picker below it, limited to {@code limit} images. Results arrive through
     * {@link #nativeImagePicked} per image and {@link #nativeImagePickFinished}
     * once, in that order; a dismissed picker finishes with no images.
     */
    public void gpuiPickImages(long requestId, int limit) {
        if (imageRequest != 0) {
            nativeImagePickFinished(requestId, HOST_ERROR, "图片选择正在进行");
            return;
        }
        imageRequest = requestId;
        imageLimit = Math.max(1, limit);
        try {
            Intent intent;
            if (Build.VERSION.SDK_INT >= 33) {
                intent = new Intent(android.provider.MediaStore.ACTION_PICK_IMAGES);
                if (limit > 1) {
                    int max = Math.min(limit, android.provider.MediaStore.getPickImagesMaxLimit());
                    intent.putExtra(android.provider.MediaStore.EXTRA_PICK_IMAGES_MAX, max);
                }
            } else {
                intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
                intent.addCategory(Intent.CATEGORY_OPENABLE);
                intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, limit > 1);
            }
            intent.setType("image/*");
            startActivityForResult(intent, REQUEST_IMAGES);
        } catch (RuntimeException error) {
            imageRequest = 0;
            nativeImagePickFinished(requestId, HOST_ERROR, errorMessage(error));
        }
    }

    private void deliverPickedImages(long request, int resultCode, Intent data, int limit) {
        java.util.ArrayList<Uri> uris = new java.util.ArrayList<>();
        if (resultCode == Activity.RESULT_OK && data != null) {
            ClipData clip = data.getClipData();
            if (clip != null) {
                for (int i = 0; i < clip.getItemCount() && uris.size() < limit; i++) {
                    Uri uri = clip.getItemAt(i).getUri();
                    if (uri != null) uris.add(uri);
                }
            } else if (data.getData() != null) {
                uris.add(data.getData());
            }
        }
        if (uris.isEmpty()) {
            nativeImagePickFinished(request, HOST_CANCELLED, null);
            return;
        }
        // Reading a content URI blocks; the picker's own thread is gone by now.
        new Thread(() -> {
            try {
                for (Uri uri : uris) {
                    byte[] bytes = readAll(uri);
                    String mime = getContentResolver().getType(uri);
                    nativeImagePicked(request, displayName(uri), mime, bytes);
                }
                nativeImagePickFinished(request, HOST_OK, null);
            } catch (Exception error) {
                nativeImagePickFinished(request, HOST_ERROR, errorMessage(error));
            }
        }, "tcode-image-pick").start();
    }

    private byte[] readAll(Uri uri) throws java.io.IOException {
        try (java.io.InputStream input = getContentResolver().openInputStream(uri)) {
            if (input == null) throw new java.io.IOException("cannot open " + uri);
            java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
            byte[] buffer = new byte[64 * 1024];
            int read;
            while ((read = input.read(buffer)) != -1) out.write(buffer, 0, read);
            return out.toByteArray();
        }
    }

    private String displayName(Uri uri) {
        try (android.database.Cursor cursor = getContentResolver().query(
                uri, new String[] {android.provider.OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                String name = cursor.getString(0);
                if (name != null && !name.isEmpty()) return name;
            }
        } catch (RuntimeException ignored) {
            // Some providers refuse metadata queries; the segment below still names the file.
        }
        String segment = uri.getLastPathSegment();
        return segment == null || segment.isEmpty() ? "image" : segment;
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == REQUEST_IMAGES) {
            long request = imageRequest;
            imageRequest = 0;
            if (request != 0) deliverPickedImages(request, resultCode, data, imageLimit);
            return;
        }
        if (requestCode != REQUEST_CAMERA) {
            return;
        }
        long request = cameraRequest;
        cameraRequest = 0;
        String value = data == null ? null : data.getStringExtra(QrScannerActivity.EXTRA_VALUE);
        String error = data == null ? null : data.getStringExtra(QrScannerActivity.EXTRA_ERROR);
        if (resultCode == Activity.RESULT_OK && value != null) {
            nativeQrScanCompleted(request, HOST_OK, value);
        } else if (error != null) {
            nativeQrScanCompleted(request, HOST_ERROR, error);
        } else {
            nativeQrScanCompleted(request, HOST_CANCELLED, "已取消扫描");
        }
    }

    /** Rust's answer to a scrolling-capture search: the timeline's window rectangle, empty to decline. */
    public void gpuiScrollCaptureBounds(long request, int left, int top, int right, int bottom) {
        if (scrollCapture != null) scrollCapture.onBounds(request, left, top, right, bottom);
    }

    /** Rust's answer to a scrolling-capture tile: how far the frame on screen is scrolled. */
    public void gpuiScrollCaptureRendered(long request, boolean ok, int scrolled) {
        if (scrollCapture != null) scrollCapture.onRendered(request, ok, scrolled);
    }

    public String gpuiReadClipboard() {
        ClipboardManager clipboard =
                (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        ClipData clip = clipboard.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) return null;
        CharSequence text = clip.getItemAt(0).coerceToText(this);
        return text == null ? null : text.toString();
    }

    public void gpuiWriteClipboard(String text) {
        ClipboardManager clipboard =
                (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        clipboard.setPrimaryClip(ClipData.newPlainText("tcode", text));
    }

    private static String errorMessage(Throwable error) {
        String message = error.getMessage();
        return message == null || message.isEmpty() ? error.getClass().getSimpleName() : message;
    }

    private final class GpuiInputView extends View {
        private int inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE;
        private int imeOptions = EditorInfo.IME_ACTION_NONE;
        private final Editable editable = new SpannableStringBuilder();
        private GpuiInputConnection connection;
        private boolean textEditable;
        private long revision;
        private long serial;

        GpuiInputView(Context context) {
            super(context);
            Selection.setSelection(editable, 0);
            if (Build.VERSION.SDK_INT >= 33) setAutoHandwritingEnabled(false);
        }

        void syncInput(long nextRevision, long acknowledgedSerial, String text,
                int selectionStart, int selectionEnd, int composingStart, int composingEnd) {
            // A frame acknowledging an earlier keystroke must not undo newer IME edits.
            // A new revision is an app edit (send, paste, cursor move, or focus change).
            if (nextRevision < revision || (nextRevision == revision && acknowledgedSerial < serial)) return;
            boolean externalEdit = nextRevision != revision;
            revision = nextRevision;
            textEditable = text != null;
            String value = textEditable ? text : "";
            if (!value.contentEquals(editable)) editable.replace(0, editable.length(), value);
            BaseInputConnection.removeComposingSpans(editable);
            if (composingStart >= 0) {
                new BaseInputConnection(this, true) {
                    @Override public Editable getEditable() { return editable; }
                }.setComposingRegion(composingStart, composingEnd);
            }
            Selection.setSelection(editable, selectionStart, selectionEnd);
            if (connection != null) connection.rememberState();
            InputMethodManager manager = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
            if (externalEdit) manager.restartInput(this);
            manager.updateSelection(this, selectionStart, selectionEnd, composingStart, composingEnd);
        }

        private void publishInput(GpuiInputConnection.State state) {
            if (!textEditable) return;
            nativeInputState(revision, ++serial, state.text(), state.selectionStart(), state.selectionEnd(),
                    state.composingStart(), state.composingEnd());
            ((InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE))
                    .updateSelection(this, state.selectionStart(), state.selectionEnd(),
                            state.composingStart(), state.composingEnd());
        }

        void configure(boolean autocorrect, int autocapitalize, boolean suggestions, int action, boolean multiLine) {
            int type = InputType.TYPE_CLASS_TEXT;
            if (multiLine) type |= InputType.TYPE_TEXT_FLAG_MULTI_LINE;
            if (autocorrect) type |= InputType.TYPE_TEXT_FLAG_AUTO_CORRECT;
            if (!suggestions) type |= InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            if (autocapitalize == 1) type |= InputType.TYPE_TEXT_FLAG_CAP_WORDS;
            if (autocapitalize == 2) type |= InputType.TYPE_TEXT_FLAG_CAP_SENTENCES;
            if (autocapitalize == 3) type |= InputType.TYPE_TEXT_FLAG_CAP_CHARACTERS;
            inputType = type;
            switch (action) {
                case 2: imeOptions = EditorInfo.IME_ACTION_DONE; break;
                case 3: imeOptions = EditorInfo.IME_ACTION_GO; break;
                case 4: imeOptions = EditorInfo.IME_ACTION_NEXT; break;
                case 5: imeOptions = EditorInfo.IME_ACTION_PREVIOUS; break;
                case 6: imeOptions = EditorInfo.IME_ACTION_SEARCH; break;
                case 7: imeOptions = EditorInfo.IME_ACTION_SEND; break;
                default: imeOptions = EditorInfo.IME_ACTION_DONE;
            }
            if (multiLine) imeOptions = EditorInfo.IME_ACTION_NONE | EditorInfo.IME_FLAG_NO_ENTER_ACTION;
        }

        @Override public boolean onCheckIsTextEditor() { return true; }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
            outAttrs.inputType = inputType;
            outAttrs.imeOptions = imeOptions | EditorInfo.IME_FLAG_NO_EXTRACT_UI;
            outAttrs.initialSelStart = Selection.getSelectionStart(editable);
            outAttrs.initialSelEnd = Selection.getSelectionEnd(editable);
            if (connection != null) connection.closeConnection();
            GpuiInputConnection.Clipboard clipboard = new GpuiInputConnection.Clipboard() {
                @Override public String read() { return gpuiReadClipboard(); }
                @Override public void write(String text) { gpuiWriteClipboard(text); }
            };
            connection = new GpuiInputConnection(this, editable, clipboard, this::publishInput) {
                @Override public boolean commitText(CharSequence text, int cursor) {
                    if (getEditable() == null) return false;
                    if (!textEditable) nativeCommitText(text.toString());
                    return super.commitText(text, cursor);
                }
                @Override public boolean setComposingText(CharSequence text, int cursor) {
                    if (getEditable() == null) return false;
                    if (!textEditable) nativeSetComposingText(text.toString());
                    return super.setComposingText(text, cursor);
                }
                @Override public boolean finishComposingText() {
                    if (getEditable() == null) return false;
                    if (!textEditable) nativeFinishComposingText();
                    return super.finishComposingText();
                }
                @Override public boolean deleteSurroundingText(int before, int after) {
                    if (getEditable() == null) return false;
                    if (!textEditable && before > 0) nativeDeleteBackward();
                    return super.deleteSurroundingText(before, after);
                }
                @Override public boolean deleteSurroundingTextInCodePoints(int before, int after) {
                    if (getEditable() == null) return false;
                    if (!textEditable && before > 0) nativeDeleteBackward();
                    return super.deleteSurroundingTextInCodePoints(before, after);
                }
                @Override public boolean sendKeyEvent(KeyEvent event) {
                    if (getEditable() == null) return false;
                    if (textEditable && event.getKeyCode() == KeyEvent.KEYCODE_DEL
                            && event.hasNoModifiers()) {
                        if (event.getAction() == KeyEvent.ACTION_DOWN) {
                            TextKeyListener.getInstance().onKeyDown(GpuiInputView.this, editable,
                                    event.getKeyCode(), event);
                            publishChanges();
                        }
                        return true;
                    }
                    forwardKeyEvent(event); return true;
                }
                @Override public boolean performEditorAction(int actionCode) {
                    if (getEditable() == null) return false;
                    nativeKeyEvent(KeyEvent.KEYCODE_ENTER, true, '\n', 0);
                    nativeKeyEvent(KeyEvent.KEYCODE_ENTER, false, '\n', 0);
                    return true;
                }
            };
            return connection;
        }

        @Override public boolean onKeyDown(int keyCode, KeyEvent event) {
            forwardKeyEvent(event); return true;
        }
        @Override public boolean onKeyUp(int keyCode, KeyEvent event) {
            forwardKeyEvent(event); return true;
        }
        @Override public boolean onTouchEvent(android.view.MotionEvent event) { return false; }

        private void forwardKeyEvent(KeyEvent event) {
            nativeKeyEvent(event.getKeyCode(), event.getAction() == KeyEvent.ACTION_DOWN,
                    event.getUnicodeChar(), event.getMetaState());
        }
    }
}
