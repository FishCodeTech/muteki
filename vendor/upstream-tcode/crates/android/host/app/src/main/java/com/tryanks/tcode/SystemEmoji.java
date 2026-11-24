package com.tryanks.tcode;

import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.RectF;
import android.graphics.fonts.Font;
import android.os.Build;
import java.io.File;
import java.io.IOException;

/** Uses Android's COLRv1 renderer for the same system font registered with cosmic-text. */
final class SystemEmoji {
    private static Font font;

    // Called on the GPUI render thread. The returned header is left, top, width, height;
    // Bitmap.getPixels supplies straight-alpha ARGB, converted to BGRA by Rust.
    static synchronized int[] rasterize(int glyph, float size) throws IOException {
        if (Build.VERSION.SDK_INT < 31 || !new File("/system/fonts/NotoColorEmoji.ttf").exists()) return null; // Older system fonts use Swash's CBDT path.
        if (font == null) font = new Font.Builder(new File("/system/fonts/NotoColorEmoji.ttf")).build();
        Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
        paint.setTextSize(size);
        RectF bounds = new RectF();
        font.getGlyphBounds(glyph, paint, bounds);
        int left = (int) Math.floor(bounds.left);
        int top = (int) Math.floor(bounds.top);
        int width = (int) Math.ceil(bounds.right) - left;
        int height = (int) Math.ceil(bounds.bottom) - top;
        if (width <= 0 || height <= 0) return new int[] {left, top, 0, 0};
        Bitmap bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888);
        try {
            new Canvas(bitmap).drawGlyphs(new int[] {glyph}, 0,
                    new float[] {-left, -top}, 0, 1, font, paint);
            int[] result = new int[4 + width * height];
            result[0] = left;
            result[1] = top;
            result[2] = width;
            result[3] = height;
            bitmap.getPixels(result, 4, width, 0, 0, width, height);
            return result;
        } finally {
            bitmap.recycle();
        }
    }
}
