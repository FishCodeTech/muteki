package com.tryanks.tcode;

import android.app.Activity;
import android.app.Application;
import android.os.Bundle;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.net.http.SslError;
import android.webkit.*;
import androidx.webkit.ProxyConfig;
import androidx.webkit.ProxyController;
import androidx.webkit.WebViewFeature;
import org.json.JSONObject;
import java.io.ByteArrayOutputStream;
import java.util.HashMap;

/** Activity-owned children. Every entry point and WebView callback runs on the Java UI thread. */
public final class PreviewHost implements Application.ActivityLifecycleCallbacks {
    private final Activity activity;
    private final HashMap<Long, TcodeWebView> views = new HashMap<>();
    private boolean resumed = true;

    private static native void nativeResult(long request, String value, byte[] png, String error);
    private static native void nativeEvent(long view, int kind, String url, String title,
                                           int code, String message);

    public PreviewHost(Activity activity) {
        this.activity = activity;
        activity.getApplication().registerActivityLifecycleCallbacks(this);
    }

    private void event(long id, int kind, String url, int code, String message) {
        WebView view = views.get(id);
        if (view != null) nativeEvent(id, kind, url == null ? "" : url,
                view.getTitle() == null ? "" : view.getTitle(), code, message);
    }

    /** A fixed JNI signature keeps owned Rust arguments independent of local JNI references. */
    public void command(long id, long request, String operation, String value,
                        int x, int y, int width, int height) {
        try {
            if (operation.equals("create")) {
                if (views.containsKey(id)) throw new IllegalStateException("duplicate preview");
                JSONObject creation = new JSONObject(value);
                String initialUrl = creation.optString("url", "about:blank");
                JSONObject proxy = creation.optJSONObject("proxy");
                String proxyOrigin = proxy == null ? null : proxy.getString("origin");
                if (proxy != null && !WebViewFeature.isFeatureSupported(WebViewFeature.PROXY_OVERRIDE))
                    throw new IllegalStateException("This Android WebView does not support remote preview proxying");
                TcodeWebView view = new TcodeWebView(activity);
                view.getSettings().setJavaScriptEnabled(true);
                view.getSettings().setDomStorageEnabled(true);
                view.getSettings().setMixedContentMode(WebSettings.MIXED_CONTENT_NEVER_ALLOW);
                view.getSettings().setAllowFileAccess(false);
                view.setWebViewClient(new WebViewClient() {
                    @Override public void onPageStarted(WebView v, String url, Bitmap icon) {
                        event(id, 0, url, 0, "");
                    }
                    @Override public void onPageFinished(WebView v, String url) {
                        event(id, 1, url, 0, "");
                    }
                    @Override public void doUpdateVisitedHistory(WebView v, String url, boolean reload) {
                        event(id, 2, url, 0, "");
                    }
                    @Override public void onReceivedError(WebView v, WebResourceRequest r, WebResourceError e) {
                        if (r.isForMainFrame()) event(id, 3, r.getUrl().toString(),
                                e.getErrorCode(), e.getDescription().toString());
                    }
                    @Override public void onReceivedHttpError(WebView v, WebResourceRequest r, WebResourceResponse e) {
                        if (r.isForMainFrame()) event(id, 4, r.getUrl().toString(),
                                e.getStatusCode(), e.getReasonPhrase());
                    }
                    @Override public void onReceivedSslError(WebView v, SslErrorHandler handler, SslError e) {
                        handler.cancel();
                        event(id, 5, e.getUrl(), e.getPrimaryError(), "TLS certificate validation failed");
                    }
                });
                view.setWebChromeClient(new WebChromeClient() {
                    @Override public void onReceivedTitle(WebView v, String title) {
                        event(id, 2, v.getUrl(), 0, "");
                    }
                });
                views.put(id, view);
                Runnable ready = () -> {
                    if (views.get(id) != view) return;
                    event(id, 7, initialUrl, 0, "");
                    view.loadUrl(initialUrl);
                };
                if (proxy != null) {
                    ProxyConfig config = new ProxyConfig.Builder().addProxyRule(proxyOrigin)
                            .removeImplicitRules().build();
                    ProxyController.getInstance().setProxyOverride(config, activity::runOnUiThread, ready);
                } else if (WebViewFeature.isFeatureSupported(WebViewFeature.PROXY_OVERRIDE)) {
                    ProxyController.getInstance().clearProxyOverride(activity::runOnUiThread, ready);
                } else ready.run();
                return;
            }
            TcodeWebView view = views.get(id);
            if (view == null) throw new IllegalStateException("preview browser is not open");
            switch (operation) {
                case "destroy":
                    blur(view);
                    views.remove(id);
                    view.destroyPreview();
                    if (views.isEmpty() && WebViewFeature.isFeatureSupported(WebViewFeature.PROXY_OVERRIDE))
                        ProxyController.getInstance().clearProxyOverride(activity::runOnUiThread, () -> {});
                    break;
                case "bounds":
                    view.setPreviewBounds(x, y, width, height);
                    break;
                case "show": view.setPreviewVisible(true); break;
                case "hide": view.setPreviewVisible(false); break;
                case "blur": blur(view); break;
                case "navigate": view.loadUrl(value); break;
                case "back": if (view.canGoBack()) view.goBack(); break;
                case "forward": if (view.canGoForward()) view.goForward(); break;
                case "reload": view.reload(); break;
                case "canGoBack": nativeResult(request, Boolean.toString(view.canGoBack()), null, null); break;
                case "evaluate":
                    view.evaluateJavascript(value, result -> nativeResult(request, result, null, null));
                    break;
                case "screenshot":
                    if (!resumed || !view.isShown() || view.getWidth() <= 0 || view.getHeight() <= 0)
                        throw new IllegalStateException("preview browser has no visible area");
                    if ((long)view.getWidth() * view.getHeight() > 16000000)
                        throw new IllegalStateException("preview screenshot exceeds 16 megapixels");
                    Bitmap bitmap = Bitmap.createBitmap(view.getWidth(), view.getHeight(), Bitmap.Config.ARGB_8888);
                    try {
                        view.draw(new Canvas(bitmap));
                        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
                        if (!bitmap.compress(Bitmap.CompressFormat.PNG, 100, bytes))
                            throw new IllegalStateException("PNG encoding failed");
                        nativeResult(request, null, bytes.toByteArray(), null);
                    } finally { bitmap.recycle(); }
                    break;
                default: throw new IllegalArgumentException("unknown preview operation " + operation);
            }
        } catch (Exception error) {
            String message = error.toString();
            if (request != 0) nativeResult(request, null, null, message);
            else nativeEvent(id, 6, "", "", 0, message);
        }
    }

    private void blur(TcodeWebView view) { view.releasePreviewFocus(); }

    @Override public void onActivityPaused(Activity a) {
        if (a != activity) return;
        resumed = false;
        for (TcodeWebView view : views.values()) view.pausePreview();
    }
    @Override public void onActivityResumed(Activity a) {
        if (a != activity) return;
        resumed = true;
        for (TcodeWebView view : views.values()) view.resumePreview();
    }
    @Override public void onActivityDestroyed(Activity a) {
        if (a != activity) return;
        for (Long id : views.keySet().toArray(new Long[0])) command(id, 0, "destroy", "", 0, 0, 0, 0);
        activity.getApplication().unregisterActivityLifecycleCallbacks(this);
    }
    @Override public void onActivityCreated(Activity a, Bundle b) {}
    @Override public void onActivityStarted(Activity a) {}
    @Override public void onActivityStopped(Activity a) {}
    @Override public void onActivitySaveInstanceState(Activity a, Bundle b) {}
}
