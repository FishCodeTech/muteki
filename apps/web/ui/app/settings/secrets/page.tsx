import { redirect } from "next/navigation";

export default function SecretSettingsRedirectPage() {
  redirect("/competitions?focus=credentials");
}
