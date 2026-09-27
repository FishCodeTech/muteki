import { redirect } from "next/navigation";

export default function CompetitionCredentialRedirectPage() {
  redirect("/competitions?focus=credentials");
}
