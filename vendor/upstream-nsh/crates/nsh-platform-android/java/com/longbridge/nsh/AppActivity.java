package com.longbridge.nsh;

import android.app.Activity;
import android.os.Bundle;

/** The activity of packaged apps (`nsh package --target android`). */
public final class AppActivity extends Activity {
    private NshView view;

    /** Created by the system from the manifest. */
    public AppActivity() {}

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        DevActivity.edgeToEdge(this);
        view = new NshView(this);
        // An app on its own, not a module embedded in another app: no limits.
        view.useStandaloneLimits();
        setContentView(view);
        if (rustApp()) view.runRustApp("nsh");
        else view.loadAssetBundle("nsh");
    }

    /** A Rust app (`nsh package --rust` sets the manifest's `com.longbridge.nsh.RUST`). */
    private boolean rustApp() {
        try {
            android.content.pm.ApplicationInfo info = getPackageManager()
                    .getApplicationInfo(getPackageName(), android.content.pm.PackageManager.GET_META_DATA);
            return info.metaData != null && info.metaData.getBoolean("com.longbridge.nsh.RUST", false);
        } catch (android.content.pm.PackageManager.NameNotFoundException e) {
            return false;
        }
    }

    @Override
    public void onConfigurationChanged(android.content.res.Configuration config) {
        super.onConfigurationChanged(config);
        DevActivity.systemBars(this);
    }

    /** Android 12 and older: the app gets the back button first. */
    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        if (view == null || !view.handleBack()) super.onBackPressed();
    }

    @Override
    protected void onDestroy() {
        view.destroy();
        super.onDestroy();
    }
}
