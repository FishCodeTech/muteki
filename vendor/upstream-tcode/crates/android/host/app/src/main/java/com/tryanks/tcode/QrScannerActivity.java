package com.tryanks.tcode;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.graphics.Color;
import android.os.Bundle;
import android.view.Gravity;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.TextView;

import androidx.annotation.NonNull;
import androidx.camera.core.CameraSelector;
import androidx.camera.core.ExperimentalGetImage;
import androidx.camera.core.ImageAnalysis;
import androidx.camera.core.ImageProxy;
import androidx.camera.core.Preview;
import androidx.camera.lifecycle.ProcessCameraProvider;
import androidx.camera.view.PreviewView;
import androidx.core.content.ContextCompat;
import androidx.fragment.app.FragmentActivity;

import com.google.common.util.concurrent.ListenableFuture;
import com.google.mlkit.vision.barcode.BarcodeScanner;
import com.google.mlkit.vision.barcode.BarcodeScannerOptions;
import com.google.mlkit.vision.barcode.BarcodeScanning;
import com.google.mlkit.vision.barcode.common.Barcode;
import com.google.mlkit.vision.common.InputImage;

import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicBoolean;

/** Full-screen CameraX + ML Kit QR scanner used by the pairing sheet. */
public final class QrScannerActivity extends FragmentActivity {
    public static final String EXTRA_VALUE = "value";
    public static final String EXTRA_ERROR = "error";

    private static final int CAMERA_PERMISSION = 6201;

    private final AtomicBoolean delivered = new AtomicBoolean();
    private final ExecutorService analysisExecutor = Executors.newSingleThreadExecutor();
    private BarcodeScanner scanner;
    private ProcessCameraProvider cameraProvider;
    private PreviewView previewView;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        buildOverlay();
        if (!getPackageManager().hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY)) {
            finishError("此设备没有可用的相机");
            return;
        }
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA)
                == PackageManager.PERMISSION_GRANTED) {
            startCamera();
        } else {
            requestPermissions(new String[] {Manifest.permission.CAMERA}, CAMERA_PERMISSION);
        }
    }

    private void buildOverlay() {
        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(Color.BLACK);
        previewView = new PreviewView(this);
        previewView.setScaleType(PreviewView.ScaleType.FILL_CENTER);
        root.addView(previewView, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));

        TextView hint = new TextView(this);
        hint.setText("将二维码置于取景框内");
        hint.setTextColor(Color.WHITE);
        hint.setTextSize(18);
        hint.setGravity(Gravity.CENTER);
        hint.setBackgroundColor(0x66000000);
        FrameLayout.LayoutParams hintLayout = new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, dp(64));
        hintLayout.gravity = Gravity.TOP;
        hintLayout.topMargin = dp(36);
        hintLayout.leftMargin = dp(24);
        hintLayout.rightMargin = dp(24);
        root.addView(hint, hintLayout);

        Button cancel = new Button(this);
        cancel.setText("取消");
        cancel.setOnClickListener(view -> {
            setResult(Activity.RESULT_CANCELED);
            finish();
        });
        FrameLayout.LayoutParams cancelLayout = new FrameLayout.LayoutParams(dp(112), dp(52));
        cancelLayout.gravity = Gravity.BOTTOM | Gravity.CENTER_HORIZONTAL;
        cancelLayout.bottomMargin = dp(48);
        root.addView(cancel, cancelLayout);
        setContentView(root);
    }

    private void startCamera() {
        BarcodeScannerOptions options = new BarcodeScannerOptions.Builder()
                .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
                .build();
        scanner = BarcodeScanning.getClient(options);
        ListenableFuture<ProcessCameraProvider> future = ProcessCameraProvider.getInstance(this);
        future.addListener(() -> {
            try {
                cameraProvider = future.get();
                if (!cameraProvider.hasCamera(CameraSelector.DEFAULT_BACK_CAMERA)) {
                    finishError("此设备没有可用的相机");
                    return;
                }
                Preview preview = new Preview.Builder().build();
                preview.setSurfaceProvider(previewView.getSurfaceProvider());
                ImageAnalysis analysis = new ImageAnalysis.Builder()
                        .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                        .build();
                analysis.setAnalyzer(analysisExecutor, this::analyze);
                cameraProvider.unbindAll();
                cameraProvider.bindToLifecycle(
                        this, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis);
            } catch (Exception error) {
                finishError("相机启动失败：" + errorMessage(error));
            }
        }, ContextCompat.getMainExecutor(this));
    }

    @androidx.annotation.OptIn(markerClass = ExperimentalGetImage.class)
    private void analyze(ImageProxy proxy) {
        if (delivered.get() || proxy.getImage() == null) {
            proxy.close();
            return;
        }
        InputImage image = InputImage.fromMediaImage(
                proxy.getImage(), proxy.getImageInfo().getRotationDegrees());
        scanner.process(image)
                .addOnSuccessListener(barcodes -> {
                    for (Barcode barcode : barcodes) {
                        String value = barcode.getRawValue();
                        if (value != null && !value.isEmpty()
                                && delivered.compareAndSet(false, true)) {
                            setResult(Activity.RESULT_OK,
                                    new Intent().putExtra(EXTRA_VALUE, value));
                            finish();
                            break;
                        }
                    }
                })
                .addOnCompleteListener(task -> proxy.close());
    }

    @Override
    public void onRequestPermissionsResult(
            int requestCode, @NonNull String[] permissions, @NonNull int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        if (requestCode != CAMERA_PERMISSION) {
            return;
        }
        if (grantResults.length > 0 && grantResults[0] == PackageManager.PERMISSION_GRANTED) {
            startCamera();
        } else {
            finishError("相机权限被拒绝，请改用手动输入");
        }
    }

    private void finishError(String message) {
        if (!delivered.compareAndSet(false, true)) {
            return;
        }
        setResult(Activity.RESULT_FIRST_USER, new Intent().putExtra(EXTRA_ERROR, message));
        finish();
    }

    @Override
    protected void onDestroy() {
        if (cameraProvider != null) {
            cameraProvider.unbindAll();
        }
        if (scanner != null) {
            scanner.close();
        }
        analysisExecutor.shutdown();
        super.onDestroy();
    }

    private int dp(int value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }

    private static String errorMessage(Throwable error) {
        String message = error.getMessage();
        return message == null || message.isEmpty() ? error.getClass().getSimpleName() : message;
    }
}
