import { redirect } from "next/navigation";
import { DEFAULT_SETTINGS_PAGE } from "@/components/settings/catalog";
export default function SettingsPage() { redirect(`/settings/${DEFAULT_SETTINGS_PAGE}`); }
