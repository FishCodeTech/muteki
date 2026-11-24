# Rust resolves these callbacks and the preview host by name through JNI.
-keepclassmembers class com.tryanks.tcode.GpuiActivity {
    public *** gpui*(...);
    public com.tryanks.tcode.PreviewHost previewHost;
}
-keep class com.tryanks.tcode.PreviewHost {
    public void command(long, long, java.lang.String, java.lang.String, int, int, int, int);
}
