import { redirect } from "next/navigation";

export default function WorkerSettingsRedirectPage() {
  redirect("/ctf/workers");
}
