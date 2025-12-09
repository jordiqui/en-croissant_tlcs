import { TlcsLivePage } from "@/components/tabs/TlcsLivePage";
import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/live")({
  component: TlcsLivePage,
});
