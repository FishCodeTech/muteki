import { notFound, redirect } from "next/navigation";
import { SettingsContent } from "@/components/settings/SettingsContent";
import { SETTINGS_PAGES, settingsPageFromPath, settingsRedirect } from "@/components/settings/catalog";
export default async function Page({ params, searchParams }: {
  params: Promise<{page: string}>;
  searchParams: Promise<Record<string, string | string[] | undefined>>;
}) {
  const {page} = await params;
  const query = new URLSearchParams();
  Object.entries(await searchParams).forEach(([key, value]) => {
    for (const part of Array.isArray(value) ? value : value === undefined ? [] : [value]) query.append(key, part);
  });
  const target = settingsRedirect(`/settings/${page}`, query);
  if (target) redirect(target);
  const id = settingsPageFromPath(`/settings/${page}`);
  if (!id || !SETTINGS_PAGES[id].platforms.includes("web")) notFound();
  return <SettingsContent page={id} />;
}
