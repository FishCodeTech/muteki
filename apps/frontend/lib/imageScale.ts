export const IMAGE_MAX_EDGE_PX = 2048;

const JPEG_EXTENSIONS = /\.(jpe?g)$/i;

export interface ScaledImage {
  file: File;
  note: string;
}

function scalableMime(type: string, name: string): boolean {
  if (type === "image/png" || type === "image/jpeg" || type === "image/webp") return true;
  return !type && /\.(png|jpe?g|webp)$/i.test(name);
}

export async function scaleImageForUpload(file: File): Promise<ScaledImage | null> {
  if (!scalableMime(file.type, file.name) || typeof createImageBitmap !== "function") return null;
  let bitmap: ImageBitmap;
  try {
    bitmap = await createImageBitmap(file);
  } catch {
    return null;
  }
  const from = { w: bitmap.width, h: bitmap.height };
  if (Math.max(from.w, from.h) <= IMAGE_MAX_EDGE_PX) {
    bitmap.close();
    return null;
  }
  const ratio = IMAGE_MAX_EDGE_PX / Math.max(from.w, from.h);
  const to = { w: Math.max(1, Math.round(from.w * ratio)), h: Math.max(1, Math.round(from.h * ratio)) };
  const canvas = document.createElement("canvas");
  canvas.width = to.w;
  canvas.height = to.h;
  const context = canvas.getContext("2d");
  if (!context) {
    bitmap.close();
    return null;
  }
  context.drawImage(bitmap, 0, 0, to.w, to.h);
  bitmap.close();
  const jpeg = file.type === "image/jpeg" || JPEG_EXTENSIONS.test(file.name);
  const blob = await new Promise<Blob | null>((resolve) =>
    canvas.toBlob(resolve, jpeg ? "image/jpeg" : "image/png", jpeg ? 0.92 : undefined));
  if (!blob) return null;
  return {
    file: new File([blob], file.name, { type: blob.type, lastModified: file.lastModified }),
    note: `已从 ${from.w}×${from.h} 缩小到 ${to.w}×${to.h} 像素再发送，以符合模型服务商接受的图片尺寸`,
  };
}
