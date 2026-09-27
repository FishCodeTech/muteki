import type { IconName } from "@/components/Icon";

export type DeviceId = "responsive" | "phone" | "tablet" | "desktop";
export type ZoomSetting = "fit" | 100 | 75 | 50;

export interface DevicePreset {
  id: DeviceId;
  label: string;
  icon: IconName;
  width: number;
  height: number;
}

export const DEVICE_PRESETS: DevicePreset[] = [
  { id: "responsive", label: "响应式", icon: "monitor", width: 0, height: 0 },
  { id: "phone", label: "手机", icon: "smartphone", width: 390, height: 844 },
  { id: "tablet", label: "平板", icon: "tablet", width: 820, height: 1180 },
  { id: "desktop", label: "桌面", icon: "monitor", width: 1280, height: 800 },
];

export const ZOOM_OPTIONS: Array<{ value: ZoomSetting; label: string }> = [
  { value: "fit", label: "适应面板" },
  { value: 100, label: "100%" },
  { value: 75, label: "75%" },
  { value: 50, label: "50%" },
];

export interface DeviceSettings {
  device: DeviceId;
  rotated: boolean;
  zoom: ZoomSetting;
}

export const DEFAULT_DEVICE: DeviceSettings = { device: "responsive", rotated: false, zoom: "fit" };

const STORAGE_KEY = "muteki.chat.preview.device.v1";

export function devicePreset(id: DeviceId): DevicePreset {
  return DEVICE_PRESETS.find((item) => item.id === id) ?? DEVICE_PRESETS[0];
}

export function deviceSize(settings: DeviceSettings): { width: number; height: number } | null {
  const preset = devicePreset(settings.device);
  if (preset.id === "responsive") return null;
  return settings.rotated ? { width: preset.height, height: preset.width } : { width: preset.width, height: preset.height };
}

export function readDeviceSettings(): DeviceSettings {
  if (typeof window === "undefined") return DEFAULT_DEVICE;
  try {
    const parsed = JSON.parse(window.localStorage.getItem(STORAGE_KEY) || "null") as Partial<DeviceSettings> | null;
    if (!parsed) return DEFAULT_DEVICE;
    const device = DEVICE_PRESETS.some((item) => item.id === parsed.device) ? (parsed.device as DeviceId) : "responsive";
    const zoom = ZOOM_OPTIONS.some((item) => item.value === parsed.zoom) ? (parsed.zoom as ZoomSetting) : "fit";
    return { device, rotated: Boolean(parsed.rotated), zoom };
  } catch {
    return DEFAULT_DEVICE;
  }
}

export function writeDeviceSettings(settings: DeviceSettings) {
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
  } catch {
    // storage blocked: settings stay per-session
  }
}
